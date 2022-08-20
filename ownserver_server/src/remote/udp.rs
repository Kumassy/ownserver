use std::{io, net::SocketAddr};
use metrics::increment_counter;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tracing::Instrument;
use tokio_util::sync::CancellationToken;
use std::sync::Arc;

use crate::{Store, remote::stream::RemoteStream, ClientStreamError};
pub use ownserver_lib::{ClientId, ControlPacket, StreamId};

use super::stream::StreamMessage;

#[tracing::instrument(skip(store, cancellation_token))]
pub async fn spawn_remote(
    store: Arc<Store>,
    listen_addr: impl ToSocketAddrs + std::fmt::Debug + Clone,
    client_id: ClientId,
    cancellation_token: CancellationToken,
) -> io::Result<()> {
    let socket = UdpSocket::bind(listen_addr.clone()).await?;
    tracing::info!(cid = %client_id, "remote process listening on {:?}", listen_addr);
    let socket = Arc::new(socket);

    let ct = cancellation_token.clone();

    tokio::spawn(
        async move {
            process_udp_stream(ct, store, client_id, socket).await;
        }
        .instrument(tracing::info_span!("process_udp_stream")),
    );

    increment_counter!("ownserver_server.remote.udp.swawn_remote");
    Ok(())
}


#[tracing::instrument(skip(ct, store, udp_socket))]
async fn process_udp_stream(
    ct: CancellationToken,
    store: Arc<Store>,
    client_id: ClientId,
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
                let remote = RemoteUdp::new(store.clone(), udp_socket.clone(), peer_addr, client_id);
                let stream_id = remote.stream_id;
                tracing::info!(cid = %client_id, sid = %remote.stream_id, "add new remote stream");
                store.add_remote(RemoteStream::RemoteUdp(remote), peer_addr).await;
                stream_id
            }
        };

        // TODO
        // gauge!("ownserver_server.remotes.udp.streams", active_streams.len() as f64);

        if n == 0 {
            tracing::debug!(cid = %client_id, sid = %stream_id, "remote client streams end");
            let _ = store
                .send_to_client(client_id, ControlPacket::End(stream_id))
                .await
                .map_err(|e| {
                    tracing::warn!(cid = %client_id, sid = %stream_id, "failed to send end signal: {:?}", e);
                });
            continue;
        }

        tracing::debug!(cid = %client_id, sid = %stream_id, "read {} bytes message from remote client", n);

        let data = &buf[..n];
        let packet = ControlPacket::UdpData(stream_id, data.to_vec());

        match store.send_to_client(client_id, packet).await {
            Ok(_) => tracing::debug!(cid = %client_id, sid = %stream_id, "sent data packet to client"),
            Err(ClientStreamError::Locked) => {
                tracing::warn!(cid = %client_id, sid = %stream_id, "client is locked");
            }
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
    socket: Arc<UdpSocket>,
    peer_addr: SocketAddr,
    ct: CancellationToken,
    store: Arc<Store>,
    disabled: bool,
}

impl RemoteUdp {
    pub fn new(store: Arc<Store>, socket: Arc<UdpSocket>, peer_addr: SocketAddr, client_id: ClientId) -> Self {
        let stream_id = StreamId::new();
        let ct = CancellationToken::new();

        Self { stream_id, client_id, socket, store, ct, peer_addr, disabled: false }
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

        if let Err(e) = self.socket.send_to(&data, self.peer_addr).await {
            tracing::warn!(sid = %self.stream_id, "could not write data to remote socket {:?}", e);

            self.disable();
            return Err(ClientStreamError::RemoteError(format!("stream_id: {}, {:?}", self.stream_id, e)))
        }
        Ok(())
    }

    pub async fn send_to_client(&self, packet: ControlPacket) -> Result<(), ClientStreamError> {
        let client_id = self.client_id;
        self.store.send_to_client(client_id, packet).await?;
        Ok(())
    }

    pub async fn send_init_to_client(&self) -> Result<(), ClientStreamError> {
        let client_id = self.client_id;
        let packet = ControlPacket::Init(self.stream_id);
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

