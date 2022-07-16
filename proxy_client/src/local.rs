use std::sync::Arc;

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{SinkExt, StreamExt};

use tokio::io::{split, AsyncReadExt, AsyncWriteExt};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;

use crate::{StreamMessage, Store};
use log::*;
use magic_tunnel_lib::{ControlPacket, StreamId};

/// Establish a new local stream and start processing messages to it
pub async fn setup_new_stream(
    store: Arc<Store>,
    local_port: u16,
    mut tunnel_tx: UnboundedSender<ControlPacket>,
    stream_id: StreamId,
) {
    info!("sid={} setting up local stream", &stream_id);

    let local_tcp = match TcpStream::connect(format!("localhost:{}", local_port)).await {
        Ok(s) => s,
        Err(e) => {
            warn!("sid={} failed to connect to local service: {:?}", stream_id, e);
            let _ = tunnel_tx.send(ControlPacket::Refused(stream_id)).await;
            return;
        }
    };
    let (stream, sink) = split(local_tcp);

    // Read local tcp bytes, send them tunnel
    let store_ = store.clone();
    tokio::spawn(async move {
        let _ = process_local_tcp(stream, tunnel_tx, stream_id).await;
        store_.remove_stream(&stream_id);
        info!("sid={} remove stream to active_streams. len={}", &stream_id, store_.len_stream());
    });

    // Forward remote packets to local tcp
    let (tx, rx) = unbounded();
    store.add_stream(stream_id, tx);
    info!("sid={} insert stream to active_streams. len={}", &stream_id, store.len_stream());

    tokio::spawn(async move {
        forward_to_local_tcp(stream_id, sink, rx).await;
        info!("sid={} end forward to local", &stream_id);
    });
}

pub async fn process_local_tcp(
    mut stream: ReadHalf<TcpStream>,
    mut tunnel: UnboundedSender<ControlPacket>,
    stream_id: StreamId,
) {
    let mut buf = [0; 4 * 1024];

    loop {
        let n = match stream.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                error!("sid={} failed to read data from socket: {:?}", &stream_id, e);
                return;
            }
        };

        if n == 0 {
            info!("sid={} done reading from client stream", &stream_id);
            return;
        }

        let data = buf[..n].to_vec();
        debug!(
            "sid={} read from local service: {}",
            &stream_id,
            data.len(),
        );

        let packet = ControlPacket::Data(stream_id, data.clone());
        if let Err(e) = tunnel.send(packet).await {
            error!("sid={} failed to tunnel packet from local tcp to tunnel: {:?}", &stream_id, e);
            return;
        }
    }
}

pub async fn forward_to_local_tcp(
    stream_id: StreamId,
    mut sink: WriteHalf<TcpStream>,
    mut queue: UnboundedReceiver<StreamMessage>,
) {
    loop {
        let data = match queue.next().await {
            Some(StreamMessage::Data(data)) => data,
            None | Some(StreamMessage::Close) => {
                warn!("sid={} closing stream", &stream_id);
                let _ = sink.shutdown().await.map_err(|e| {
                    error!("sid={} failed to shutdown: {:?}", &stream_id, e);
                });
                return;
            }
        };

        if let Err(e) = sink.write_all(&data).await {
            error!("sid={} failed to write packet data to local tcp socket: {:?}", &stream_id, e);
            return;
        }
        debug!("sid={} wrote to local service: {}", &stream_id, data.len());
    }
}