use metrics::increment_counter;
use ownserver_lib::{EndpointId, ControlPacketV2};
use std::io::{self, ErrorKind};
use std::sync::Arc;
use tokio::{net::{TcpListener, TcpStream}, io::{WriteHalf, AsyncReadExt, AsyncWriteExt}};
use tracing::Instrument;
use tokio_util::sync::CancellationToken;

use crate::{ClientStreamError, Store, remote::stream::RemoteStream};
pub use ownserver_lib::{ClientId, StreamId};

use super::stream::StreamMessage;

#[tracing::instrument(skip(store, cancellation_token))]
pub async fn spawn_remote(
    store: Arc<Store>,
    client_id: ClientId,
    endpoint_id: EndpointId,
    cancellation_token: CancellationToken,
) -> io::Result<()> {
    // create our accept any server
    let listen_addr = store.get_remote_addr_by_endpoint_id(endpoint_id).ok_or(io::Error::from(ErrorKind::Other))?;
    let listener = TcpListener::bind(listen_addr.clone()).await?;
    tracing::info!(cid = %client_id, eid = %endpoint_id, "remote process listening on {:?}", listen_addr);

    let ct = cancellation_token.clone();

    tokio::spawn(async move {
        loop {
            let socket = tokio::select! {
                socket = listener.accept() => {
                    match socket {
                        Ok((socket, _)) => socket,
                        _ => {
                            tracing::debug!(cid = %client_id, eid = %endpoint_id, "failed to accept socket");
                            continue;
                        }
                    }
                },
                _ = ct.cancelled() => {
                    tracing::info!(cid = %client_id, eid = %endpoint_id, "tcp listener is cancelled.");
                    return;
                },
            };

            let store_ = store.clone();

            tokio::spawn(
                async move {
                    accept_connection(store_, socket, client_id, endpoint_id).await;
                }
                .instrument(tracing::info_span!("remote_connect")),
            );
        }
    }.instrument(tracing::info_span!("spawn_accept_connection")));

    increment_counter!("ownserver_server.remote.tcp.swawn_remote");
    Ok(())
}

#[tracing::instrument(skip(store, socket))]
pub async fn accept_connection(
    store: Arc<Store>,
    socket: TcpStream,
    client_id: ClientId,
    endpoint_id: EndpointId,
) {
    tracing::info!(cid = %client_id, "new remote connection");

    let peer_addr = match socket.peer_addr() {
        Ok(addr) => addr,
        Err(_) => {
            tracing::error!(cid = %client_id, "failed to find remote peer addr");
            // let _ = socket.write_all(HTTP_TUNNEL_REFUSED_RESPONSE).await;
            return;
        }
    };
    tracing::info!(cid = %client_id, "remote ip is {}", peer_addr);


    let remote = RemoteTcp::new(store.clone(), socket, client_id, endpoint_id);
    if remote.send_init_to_client().await.is_ok() {
        tracing::info!(cid = %client_id, sid = %remote.stream_id, "add new remote stream");
        store.add_remote(RemoteStream::RemoteTcp(remote), peer_addr).await;
    }
}



#[derive(Debug)]
pub struct RemoteTcp {
    pub stream_id: StreamId,
    pub client_id: ClientId,
    pub endpoint_id: EndpointId,
    socket_tx: WriteHalf<TcpStream>,
    ct: CancellationToken,
    store: Arc<Store>,
    disabled: bool,
}

impl RemoteTcp {
    pub fn new(store: Arc<Store>, socket: TcpStream, client_id: ClientId, endpoint_id: EndpointId) -> Self {
        let (mut stream, sink) = tokio::io::split(socket);
        let stream_id = StreamId::new();
        let ct: CancellationToken = CancellationToken::new();

        let mut buf = [0; 4096];
        let ct_ = ct.clone();
        let store_ = store.clone();
        tokio::spawn(async move {
            loop {
                let n = tokio::select! {
                    read = stream.read(&mut buf) => {
                        match read {
                            Ok(n) => n,
                            Err(e) => {
                                tracing::warn!(cid = %client_id, sid = %stream_id, "failed to read from tcp socket: {:?}", e);
        
                                // error: clean up this remote stream
                                break
                            }
                        }
                    }
                    _ = ct_.cancelled() => {
                        // exit from this remote stream
                        tracing::info!(cid = %client_id, id=%stream_id, "read loop was cancelled");
                        return;
                    }
                };
                tracing::debug!(cid = %client_id, sid = %stream_id, "read {} bytes message from remote", n);

                if n == 0 {
                    tracing::debug!(cid = %client_id, sid = %stream_id, "remote client streams end");


                    let _ = store_ 
                        .send_to_client(client_id, ControlPacketV2::End(stream_id))
                        .await
                        .map_err(|e| {
                            tracing::warn!(cid = %client_id, sid = %stream_id, "failed to send end signal: {:?}", e);
                        });
                    // safely close this remote stream
                    break
                }

                let data = &buf[..n];
                let packet = ControlPacketV2::Data(stream_id, data.to_vec());

                match store_.send_to_client(client_id, packet).await {
                    Ok(_) => {
                        tracing::debug!(cid = %client_id, sid = %stream_id, "sent data packet to client");
                    },
                    Err(e) => {
                        tracing::warn!(cid = %client_id, sid = %stream_id, "failed to forward tcp packets to client. {:?}", e);
                        // error: client is unavailable or error
                        // error: clean up this remote stream
                        break
                    }
                }
            }

            tracing::info!(cid = %client_id, sid = %stream_id, "exit from read loop");
            store_.disable_remote(stream_id).await;
        }.instrument(tracing::info_span!("remote_tcp_read_loop")));

        Self { stream_id, client_id, endpoint_id, socket_tx: sink, store, ct, disabled: false }
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

                let _ = self.socket_tx.shutdown().await.map_err(|_e| {
                    tracing::error!(sid = %self.stream_id, "error shutting down remote tcp stream");
                });

                self.disable();
                return Err(ClientStreamError::RemoteError(format!("stream_id: {}, TunnelRefused", self.stream_id)))
            }
            StreamMessage::NoClientTunnel => {
                unimplemented!();
            }
        };

        if let Err(e) = self.socket_tx.write_all(&data).await {
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
        let packet = ControlPacketV2::Init(self.stream_id, self.endpoint_id);
        self.store.send_to_client(client_id, packet).await?;
        Ok(())
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }
    pub fn disable(&mut self) {
        tracing::info!(sid = %self.stream_id, "tcp stream was disabled");
        self.ct.cancel();
        self.disabled = true;
    }
}

