use std::io::{self, ErrorKind};
use std::sync::Arc;

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{SinkExt, StreamExt};

use tokio::net::UdpSocket;

use crate::{StreamMessage, Store};
use log::*;
use ownserver_lib::{StreamId, EndpointId, ControlPacketV2};

/// Establish a new local stream and start processing messages to it
pub async fn setup_new_stream(
    store: Arc<Store>,
    tunnel_tx: UnboundedSender<ControlPacketV2>,
    stream_id: StreamId,
    endpoint_id: EndpointId,
) -> io::Result<()>{
    info!("sid={} eid={} setting up local udp stream", stream_id, endpoint_id);
    let local_addr = store.get_local_addr_by_endpoint_id(endpoint_id).ok_or(io::Error::from(ErrorKind::Other))?;

    let local_udp = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => {
            warn!("sid={} eid={} failed to bind socket: {:?}", stream_id, endpoint_id, e);
            return Err(e);
        } 
    };
    debug!("local udp addr: {:?}", local_udp.local_addr());

    if let Err(e) = local_udp.connect(local_addr).await {
        warn!("sid={} failed to bind socket: {:?}", &stream_id, e);
        return Err(e);
    }

    let local_udp = Arc::new(local_udp);

    // Read local tcp bytes, send them tunnel
    let store_ = store.clone();
    let local_udp_ = local_udp.clone();
    tokio::spawn(async move {
        let _ = process_local_udp(local_udp_, tunnel_tx, stream_id).await;
        store_.remove_stream(&stream_id);
        info!("sid={} remove stream to active_streams. len={}", &stream_id, store_.len_stream());
    });

    // Forward remote packets to local tcp
    let (tx, rx) = unbounded();
    store.add_stream(stream_id, tx);
    info!("sid={} insert stream to active_streams. len={}", &stream_id, store.len_stream());

    tokio::spawn(async move {
        forward_to_local_udp(stream_id, local_udp, rx).await;
        info!("sid={} end forward to local", &stream_id);
    });

    Ok(())
}

pub async fn process_local_udp(
    // mut stream: ReadHalf<TcpStream>,
    stream: Arc<UdpSocket>,
    mut tunnel: UnboundedSender<ControlPacketV2>,
    stream_id: StreamId,
) {
    let mut buf = [0; 4 * 1024];

    loop {
        // let n = match stream.read(&mut buf).await {
        let n = match stream.recv(&mut buf).await {
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
        if let Err(e) = tunnel.send(packet).await {
            error!("sid={} failed to tunnel packet from local tcp to tunnel: {:?}", &stream_id, e);
            return;
        }
    }
}

pub async fn forward_to_local_udp(
    stream_id: StreamId,
    // mut sink: WriteHalf<TcpStream>,
    sink: Arc<UdpSocket>,
    mut queue: UnboundedReceiver<StreamMessage>,
) {
    loop {
        let data = match queue.next().await {
            Some(StreamMessage::Data(data)) => data,
            None | Some(StreamMessage::Close) => {
                warn!("sid={} closing stream", &stream_id);
                // let _ = sink.shutdown().await.map_err(|e| {
                //     error!("sid={} failed to shutdown: {:?}", &stream_id, e);
                // });
                return;
            }
        };

        // if let Err(e) = sink.write_all(&data).await {
        if let Err(e) = sink.send(&data).await {
            error!("sid={} failed to write packet data to local tcp socket: {:?}", &stream_id, e);
            return;
        }
        debug!("sid={} wrote to local service: {}", &stream_id, data.len());
    }
}