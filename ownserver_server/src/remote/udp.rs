use std::{io::{self, ErrorKind}, net::SocketAddr};
use metrics::counter;
use ownserver_lib::{ControlPacketV2, EndpointId, RemoteInfo};
use tokio::net::UdpSocket;
use tracing::Instrument;
use tokio_util::sync::CancellationToken;
use warp::filters::ws::WebSocket;
use std::sync::Arc;

use crate::{Store, remote::stream::RemoteStream, ClientStreamError};
pub use ownserver_lib::{ClientId, StreamId};

use super::stream::StreamMessage;

#[tracing::instrument(skip(store, cancellation_token))]
pub async fn spawn_remote(
    store: Arc<Store<WebSocket>>,
    client_id: ClientId,
    endpoint_id: EndpointId,
    cancellation_token: CancellationToken,
) -> io::Result<()> {
    let listen_addr = store.get_remote_addr_by_endpoint_id(endpoint_id).ok_or(io::Error::from(ErrorKind::Other))?;
    let socket = UdpSocket::bind(listen_addr.clone()).await?;
    tracing::info!(cid = %client_id, eid = %endpoint_id, "remote process listening on {:?}", listen_addr);
    let socket = Arc::new(socket);

    let ct = cancellation_token.clone();

    tokio::spawn(
        async move {
            process_udp_stream(ct, store, client_id, endpoint_id, socket).await;
        }
        .instrument(tracing::info_span!("process_udp_stream")),
    );

    counter!("ownserver_server.remote.udp.swawn_remote").increment(1);
    Ok(())
}


#[tracing::instrument(skip(ct, store, udp_socket))]
async fn process_udp_stream(
    ct: CancellationToken,
    store: Arc<Store<WebSocket>>,
    client_id: ClientId,
    endpoint_id: EndpointId,
    udp_socket: Arc<UdpSocket>,
)
{
    let mut buf = [0; 4096];
    loop {
        let (n, peer_addr) = tokio::select! {
            read = udp_socket.recv_from(&mut buf) => {
                match read {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(cid = %client_id, "failed to read from remote udp socket: {:?}", e);
                        continue;
                    }
                }
            }
            _ = ct.cancelled() => {
                // exit from this remote stream
                tracing::info!(cid = %client_id, "process_udp_stream was cancelled");
                return;
            }
        };

        let stream_id = match store.find_stream_id_by_addr(&peer_addr).await {
            Some(stream_id) => stream_id,
            None => {
                tracing::info!(cid = %client_id, "remote ip is {}", peer_addr);
                let remote = RemoteUdp::new(store.clone(), udp_socket.clone(), peer_addr, client_id, endpoint_id);
                let stream_id = remote.stream_id;
                if remote.send_init_to_client().await.is_ok() {
                    tracing::info!(cid = %client_id, sid = %remote.stream_id, "add new remote stream");
                    store.add_remote(RemoteStream::RemoteUdp(remote), peer_addr).await;
                } else {
                    tracing::warn!(cid = %client_id, sid = %remote.stream_id, "failed to send init packet to client");
                    return;
                }
                stream_id
            }
        };

        // TODO
        // gauge!("ownserver_server.remotes.udp.streams", active_streams.len() as f64);

        if n == 0 {
            tracing::debug!(cid = %client_id, sid = %stream_id, "remote client streams end");
            let _ = store
                .send_to_client(client_id, ControlPacketV2::End(stream_id))
                .await
                .map_err(|e| {
                    tracing::warn!(cid = %client_id, sid = %stream_id, "failed to send end signal: {:?}", e);
                });
            continue;
        }

        tracing::debug!(cid = %client_id, sid = %stream_id, "read {} bytes message from remote client", n);

        let data = &buf[..n];
        let packet = ControlPacketV2::Data(stream_id, data.to_vec());

        match store.send_to_client(client_id, packet).await {
            Ok(_) => tracing::debug!(cid = %client_id, sid = %stream_id, "sent data packet to client"),
            Err(_) => {
                tracing::warn!(cid = %client_id, sid = %stream_id, "failed to forward udp packets to client");
                continue
            }
        }
    }
}


#[derive(Debug)]
pub struct RemoteUdp {
    pub stream_id: StreamId,
    pub client_id: ClientId,
    pub endpoint_id: EndpointId,
    socket: Arc<UdpSocket>,
    remote_info: RemoteInfo,
    ct: CancellationToken,
    store: Arc<Store<WebSocket>>,
    disabled: bool,
}

impl RemoteUdp {
    pub fn new(store: Arc<Store<WebSocket>>, socket: Arc<UdpSocket>, peer_addr: SocketAddr, client_id: ClientId, endpoint_id: EndpointId) -> Self {
        let stream_id = StreamId::new();
        let ct = CancellationToken::new();
        let remote_info = RemoteInfo::new(peer_addr);

        Self { stream_id, client_id, endpoint_id, socket, store, ct, remote_info, disabled: false }
    }

    pub async fn send_to_remote(&mut self, stream_id: StreamId, message: StreamMessage) -> Result<(), ClientStreamError> {
        if self.disabled() {
            return Err(ClientStreamError::StreamNotAvailable(stream_id));
        }

        let data = match message {
            StreamMessage::Data(data) => data,
            StreamMessage::TunnelRefused => {
                // TODO
                tracing::info!(sid = %self.stream_id, "tunnel refused");

                self.disable();
                return Err(ClientStreamError::RemoteError(format!("stream_id: {}, TunnelRefused", self.stream_id)))
            }
            StreamMessage::NoClientTunnel => {
                unimplemented!();
            }
        };

        if let Err(e) = self.socket.send_to(&data, self.remote_info.remote_peer_addr).await {
            tracing::warn!(sid = %self.stream_id, "could not write data to remote socket {:?}", e);

            self.disable();
            return Err(ClientStreamError::RemoteError(format!("stream_id: {}, {:?}", self.stream_id, e)))
        }
        Ok(())
    }

    pub async fn send_to_client(&self, packet: ControlPacketV2) -> Result<(), ClientStreamError> {
        let client_id = self.client_id;
        self.store.send_to_client(client_id, packet).await?;
        Ok(())
    }

    pub async fn send_init_to_client(&self) -> Result<(), ClientStreamError> {
        let client_id = self.client_id;
        let packet = ControlPacketV2::Init(self.stream_id, self.endpoint_id, self.remote_info.clone());
        self.store.send_to_client(client_id, packet).await?;
        Ok(())
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }
    pub fn disable(&mut self) {
        tracing::info!(sid = %self.stream_id, "udp stream was disabled");
        self.ct.cancel();
        self.disabled = true;
    }
}

