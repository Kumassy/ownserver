use std::io::{self, ErrorKind};
use std::sync::Arc;

use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use futures::StreamExt;

use tokio::io::{split, AsyncReadExt, AsyncWriteExt};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;

use crate::{LocalStream, Store, StreamMessage};
use log::*;
use ownserver_lib::{ControlPacketV2, EndpointId, RemoteInfo, StreamId};

/// Establish a new local stream and start processing messages to it
pub async fn setup_new_stream(
    store: Arc<Store>,
    stream_id: StreamId,
    endpoint_id: EndpointId,
    remote_info: RemoteInfo,
) -> io::Result<()> {
    info!("sid={} eid={} setting up local tcp stream", stream_id, endpoint_id);
    let local_addr = store.get_local_addr_by_endpoint_id(endpoint_id).ok_or(io::Error::from(ErrorKind::Other))?;

    let local_tcp = match TcpStream::connect(local_addr).await {
        Ok(s) => s,
        Err(e) => {
            warn!("sid={} eid={} failed to connect to local service: {:?}", stream_id, endpoint_id, e);
            let _ = store.send_to_server(ControlPacketV2::Refused(stream_id)).await;
            return Err(e);
        }
    };
    let (stream, sink) = split(local_tcp);

    // Read local tcp bytes, send them tunnel
    let store_ = store.clone();
    tokio::spawn(async move {
        let _ = process_local_tcp(store_.clone(), stream, stream_id).await;
        store_.remove_stream(&stream_id);
        info!("sid={} remove stream to active_streams. len={}", &stream_id, store_.len_stream());
    });

    // Forward remote packets to local tcp
    let (tx, rx) = unbounded();
    let local_stream = LocalStream::new(tx, remote_info);
    store.add_stream(stream_id, local_stream);
    info!("sid={} insert stream to active_streams. len={}", &stream_id, store.len_stream());

    tokio::spawn(async move {
        forward_to_local_tcp(stream_id, sink, rx).await;
        info!("sid={} end forward to local", &stream_id);
    });

    Ok(())
}

pub async fn process_local_tcp(
    store: Arc<Store>,
    mut stream: ReadHalf<TcpStream>,
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

        let packet = ControlPacketV2::Data(stream_id, data.clone());
        if let Err(e) = store.send_to_server(packet).await {
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