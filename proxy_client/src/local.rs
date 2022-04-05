use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{SinkExt, StreamExt};

use tokio::io::{split, AsyncReadExt, AsyncWriteExt};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;

use crate::{ActiveStreams, StreamMessage};
use log::*;
use magic_tunnel_lib::{ControlPacket, StreamId};

/// Establish a new local stream and start processing messages to it
pub async fn setup_new_stream(
    active_streams: ActiveStreams,
    local_port: u16,
    mut tunnel_tx: UnboundedSender<ControlPacket>,
    stream_id: StreamId,
) {
    info!("sid={} setting up local stream", &stream_id.to_string());

    let local_tcp = match TcpStream::connect(format!("localhost:{}", local_port)).await {
        Ok(s) => s,
        Err(e) => {
            warn!("sid={} failed to connect to local service: {:?}", &stream_id.to_string(), e);
            let _ = tunnel_tx.send(ControlPacket::Refused(stream_id)).await;
            return;
        }
    };
    let (stream, sink) = split(local_tcp);

    // Read local tcp bytes, send them tunnel
    let stream_id_clone = stream_id.clone();
    let active_streams_clone = active_streams.clone();
    tokio::spawn(async move {
        let active_streams = active_streams_clone;
        let stream_id = stream_id_clone;
        let _ = process_local_tcp(stream, tunnel_tx, stream_id.clone()).await;
        active_streams
            .write()
            .unwrap()
            .remove(&stream_id);
        info!("sid={} remove stream to active_streams. len={}", &stream_id.to_string(), active_streams.read().unwrap().len());
    });

    // Forward remote packets to local tcp
    let (tx, rx) = unbounded();
    active_streams
        .write()
        .unwrap()
        .insert(stream_id.clone(), tx.clone());
    info!("sid={} insert stream to active_streams. len={}", &stream_id.to_string(), active_streams.read().unwrap().len());

    tokio::spawn(async move {
        forward_to_local_tcp(stream_id.clone(), sink, rx).await;
        info!("sid={} end forward to local", &stream_id.to_string());
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
                error!("sid={} failed to read data from socket: {:?}", &stream_id.to_string(), e);
                return;
            }
        };

        if n == 0 {
            info!("sid={} done reading from client stream", &stream_id.to_string());
            return;
        }

        let data = buf[..n].to_vec();
        debug!(
            "sid={} read from local service: {}",
            &stream_id.to_string(),
            data.len(),
        );

        let packet = ControlPacket::Data(stream_id.clone(), data.clone());
        if let Err(e) = tunnel.send(packet).await {
            error!("sid={} failed to tunnel packet from local tcp to tunnel: {:?}", &stream_id.to_string(), e);
            return;
        }
    }
}

async fn forward_to_local_tcp(
    stream_id: StreamId,
    mut sink: WriteHalf<TcpStream>,
    mut queue: UnboundedReceiver<StreamMessage>,
) {
    loop {
        let data = match queue.next().await {
            Some(StreamMessage::Data(data)) => data,
            None | Some(StreamMessage::Close) => {
                warn!("sid={} closing stream", &stream_id.to_string());
                let _ = sink.shutdown().await.map_err(|e| {
                    error!("sid={} failed to shutdown: {:?}", &stream_id.to_string(), e);
                });
                return;
            }
        };

        if let Err(e) = sink.write_all(&data).await {
            error!("sid={} failed to write packet data to local tcp socket: {:?}", &stream_id.to_string(), e);
            return;
        }
        debug!("sid={} wrote to local service: {}", &stream_id.to_string(), data.len());
    }
}