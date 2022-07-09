use std::{net::SocketAddr, sync::Arc};

use active_stream::{StreamMessage, ActiveStream};
use dashmap::DashMap;
use futures::{channel::mpsc::SendError, StreamExt, stream::{SplitSink, SplitStream}, SinkExt, future::Remote};
use magic_tunnel_lib::{ClientId, StreamId, ControlPacket};
use metrics::gauge;
use thiserror::Error;

pub mod active_stream;
pub mod connected_clients;
pub mod control_server;
pub mod remote;
pub mod proxy_server;
pub mod port_allocator;

#[derive(Debug, Clone)]
pub struct Config {
    pub control_port: u16,
    pub token_secret: String,
    pub host: String,
    pub remote_port_start: u16,
    pub remote_port_end: u16,
}


#[derive(Error, Debug, PartialEq)]
pub enum ProxyServerError {
    #[error("Failed to load config because it is not initialized.")]
    ConfigNotInitialized,
}

#[derive(Error, Debug, PartialEq)]
pub enum ForwardingError {
    #[error("Destination is disabled.")]
    DestinationDisabled,
    #[error("Failed to put data into sender buffer.")]
    SendError(#[from] SendError),
}

use tokio::{net::{TcpStream, UdpSocket}, io::{WriteHalf, AsyncReadExt, AsyncWriteExt}, select};
use tokio_util::sync::CancellationToken;
use warp::{
    ws::{Message, WebSocket, Ws},
    Error as WarpError, Filter,
};

#[derive(Debug)]
pub struct Client {
    pub client_id: ClientId,
    pub host: String,

    ws_tx: SplitSink<WebSocket, Message>,
    // ws_rx: SplitStream<WebSocket>,
    store: Arc<Store>,
    ct: CancellationToken,
    disabled: bool,
}

impl Client {
    pub fn new(store: Arc<Store>, client_id: ClientId, host: String, ws: WebSocket) -> Self {
        let (sink, mut stream) = ws.split();
        let token = CancellationToken::new();

        let ct = token.clone();
        let store_ = store.clone();
        tokio::spawn(async move {
            loop {
                select! {
                    _ = ct.cancelled() => {
                        break
                    }
                    result = stream.next() => {
                        let message = match result {
                            // handle protocol message
                            Some(Ok(msg)) if (msg.is_binary() || msg.is_text()) && !msg.as_bytes().is_empty() => {
                                msg.into_bytes()
                            }
                            // handle close with reason
                            Some(Ok(msg)) if msg.is_close() && !msg.as_bytes().is_empty() => {
                                tracing::info!(cid = %client_id, close_reason=?msg, "client got close");
                                break
                            }
                            _ => {
                                tracing::info!(cid = %client_id, "goodbye client");
                                break
                            }
                        };
                
                        let packet: ControlPacket = match rmp_serde::from_slice(&message) {
                            Ok(packet) => packet,
                            Err(e) => {
                                tracing::warn!(cid = %client_id, error = ?e, "failed to parse client message");
                                continue;
                            }
                        };
                
                        tracing::trace!(cid = %client_id, ?packet, "got control packet from client");

                        let (stream_id, message) = match packet {
                            ControlPacket::Data(stream_id, data) => {
                                tracing::trace!(cid = %client_id, sid = %stream_id, "forwarding to stream: {}", data.len());
                                (stream_id, StreamMessage::Data(data))
                            }
                            ControlPacket::Refused(stream_id) => {
                                tracing::debug!(cid = %client_id, sid = %stream_id, "tunnel says: refused");
                                (stream_id, StreamMessage::TunnelRefused)
                            }
                            ControlPacket::Init(stream_id) | ControlPacket::End(stream_id) => {
                                tracing::error!(cid = %client_id, sid = %stream_id, "invalid protocol control::init message");
                                continue;
                            }
                            ControlPacket::Ping => {
                                tracing::trace!(cid = %client_id, "pong");
                                continue;
                            }
                            ControlPacket::UdpData(stream_id, data) => {
                                tracing::trace!(cid = %client_id, sid = %stream_id, "forwarding udp to stream: {}", data.len());
                                (stream_id, StreamMessage::Data(data))
                            }
                        };

                        tracing::trace!(cid = %client_id, sid = %stream_id, "forward message to remote stream");
                        if let Err(e) = store_.send_to_remote(stream_id, message).await {
                            tracing::debug!(cid = %client_id, sid = %stream_id, error = ?e, "Failed to send to remote stream");

                            store_.disable_remote(stream_id);
                        }
                    }
                }
            }
            store_.disable_client(client_id);
        });

        Self { client_id, host, ws_tx: sink, store, ct: token, disabled: false }
    }

    // pub async fn send_to_stream(&self, stream_id: StreamId, message: StreamMessage) -> Result<(), Box<dyn std::error::Error>> {
    //     self.store.streams.get_mut(&stream_id).unwrap().send_to_remote(stream_id, message).await?;
    //     Ok(())
    // }

    pub async fn send_to_client(&mut self, packet: ControlPacket) -> Result<(), ClientStreamError> {
        let data = match rmp_serde::to_vec(&packet) {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(cid = %self.client_id, error = ?error, "failed to encode message");
                return Err(ClientStreamError::ClientError(format!("packet is invalid {}", error)))
            }
        };

        if let Err(e) =  self.ws_tx.send(Message::binary(data)).await {
            tracing::debug!(cid = %self.client_id, error = ?e, "client disconnected: aborting");
            self.disable();
            return Err(ClientStreamError::ClientError(format!("failed to communicate with client {:?}", e)))
        }
        Ok(())
    }

    pub fn disable(&mut self) {
        self.ct.cancel();
        self.disabled = true;
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.ct.clone()
    }

}

#[derive(Debug)]
pub enum RemoteStream {
    RemoteTcp(RemoteTcp),
    RemoteUdp,
}

// #[async_trait]
// trait RemoteStream {
//     async fn send_to_remote(&mut self, stream_id: StreamId, message: StreamMessage) -> Result<(), Box<dyn std::error::Error>>;
//     async fn send_to_client(&self, packet: ControlPacket) -> Result<(), Box<dyn std::error::Error>>;
// }


impl RemoteStream {
    pub async fn send_to_remote(&mut self, stream_id: StreamId, message: StreamMessage) -> Result<(), ClientStreamError> {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.send_to_remote(stream_id, message).await
            }
            _ => {
                unimplemented!();
            }
        }
    }

    pub async fn send_to_client(&self, packet: ControlPacket) -> Result<(), ClientStreamError> {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.send_to_client(packet).await
            }
            _ => {
                unimplemented!();
            }
        }
    }


    pub fn disable(&mut self) {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.disable();
            }
            _ => {
                unimplemented!();
            }
        }
    }

    pub fn disabled(&self) -> bool {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.disabled()
            }
            _ => {
                unimplemented!();
            }
        }
    }

    pub fn stream_id(&self) -> StreamId {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.stream_id
            }
            _ => {
                unimplemented!();
            }
        }
    }
}

#[derive(Debug)]
pub struct RemoteTcp {
    pub stream_id: StreamId,
    pub client_id: ClientId,
    socket_tx: WriteHalf<TcpStream>,
    ct: CancellationToken,
    store: Arc<Store>,
    disabled: bool,
}

impl RemoteTcp {
    pub fn new(store: Arc<Store>, socket: TcpStream, client_id: ClientId) -> Self {
        let (mut stream, sink) = tokio::io::split(socket);
        let stream_id = StreamId::new();
        let ct = CancellationToken::new();

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
                        .send_to_client(client_id, ControlPacket::End(stream_id))
                        .await
                        .map_err(|e| {
                            tracing::warn!(cid = %client_id, sid = %stream_id, "failed to send end signal: {:?}", e);
                        });
                    // safely close this remote stream
                }

                let data = &buf[..n];
                let packet = ControlPacket::Data(stream_id, data.to_vec());

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
            store_.disable_remote(stream_id);
        });

        Self { stream_id, client_id, socket_tx: sink, store, ct, disabled: false }
    }

    async fn send_to_remote(&mut self, stream_id: StreamId, message: StreamMessage) -> Result<(), ClientStreamError> {
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
    fn disable(&mut self) {
        self.ct.cancel();
        self.disabled = true;
    }
}

#[derive(Error, Debug, PartialEq)]
pub enum ClientStreamError {
    #[error("Failed to send data to client {0}.")]
    ClientError(String),
    #[error("Failed to send data to remote.")]
    RemoteError(String),
    #[error("Client {0} is not registered to Store or no longer available.")]
    ClientNotAvailable(ClientId),
    #[error("Stream {0} is not registered to Store or no longer available.")]
    StreamNotAvailable(StreamId),
    #[error("Remote stream has closed.")]
    RemoteEnd,
}

// #[async_trait]
// impl RemoteStream for RemoteTcp {
//     async fn send_to_remote(&mut self, stream_id: StreamId, message: StreamMessage) -> Result<(), Box<dyn std::error::Error>> {
//         self.socket_tx.write_all(&b"hogehoge".to_vec()).await?;
//         Ok(())
//     }

//     async fn send_to_client(&self, packet: ControlPacket) -> Result<(), Box<dyn std::error::Error>> {
//         Ok(())
//     }
// }


#[derive(Debug)]
pub struct RemoteUdp {
    pub id: StreamId,
    pub client_id: ClientId,
    socket: Arc<UdpSocket>,
    store: Arc<Store>,
}


#[derive(Debug, Default)]
pub struct Store {
    // pub streams: DashMap<StreamId, Arc<dyn RemoteStream>>,
    pub streams: DashMap<StreamId, RemoteStream>,
    pub addrs_map: DashMap<SocketAddr, StreamId>,

    pub clients: DashMap<ClientId, Client>,
    pub hosts_map: DashMap<String, ClientId>,
}

impl Store {
    pub async fn send_to_client(&self, client_id: ClientId, packet: ControlPacket) -> Result<(), ClientStreamError> {
        match self.clients.get_mut(&client_id) {
            Some(mut client) => {
                client.send_to_client(packet).await
            },
            None => {
                Err(ClientStreamError::ClientNotAvailable(client_id))
            }
        }
    }

    pub async fn send_to_remote(&self, stream_id: StreamId, message: StreamMessage) -> Result<(), ClientStreamError> {
        match self.streams.get_mut(&stream_id) {
            Some(mut stream) => {
                stream.send_to_remote(stream_id, message).await
            },
            None => {
                Err(ClientStreamError::StreamNotAvailable(stream_id))
            }
        }
    }

    pub fn disable_remote(&self, stream_id: StreamId) {
        if let Some(mut stream) = self.streams.get_mut(&stream_id) {
            stream.disable();
        }
    }

    pub fn disable_client(&self, client_id: ClientId) {
        if let Some(mut client) = self.clients.get_mut(&client_id) {
            client.disable();
        }
    }

    pub fn add_client(&self, client: Client) {
        let client_id = client.client_id;
        let host = client.host.clone();
        self.clients.insert(client_id, client);
        self.hosts_map.insert(host, client_id);
        gauge!("magic_tunnel_server.control.connections", self.clients.len() as f64);
    }

    pub fn add_remote(&self, remote: RemoteStream) {
        let stream_id = remote.stream_id();
        self.streams.insert(stream_id, remote);
    }

    pub fn cleanup(&self) {
        self.streams.retain(|_, v| !v.disabled());
        self.clients.retain(|_, v| !v.disabled());
    }
}