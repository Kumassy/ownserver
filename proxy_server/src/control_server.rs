use futures::{
    channel::mpsc::{unbounded, SendError},
    Sink, SinkExt, Stream, StreamExt,
};
use magic_tunnel_lib::Payload;
pub use magic_tunnel_lib::{ClientHello, ClientId, ControlPacket, ServerHello, StreamId, CLIENT_HELLO_VERSION};
use magic_tunnel_auth::decode_jwt;
use metrics::increment_counter;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::task::JoinHandle;
use tracing::Instrument;
use warp::{
    ws::{Message, WebSocket, Ws},
    Error as WarpError, Filter,
};

use dashmap::DashMap;
use rand::{rngs::StdRng, SeedableRng};
use std::ops::Range;
use std::sync::Arc;
use tokio::sync::Mutex;
use once_cell::sync::OnceCell;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::active_stream::{ActiveStream, ActiveStreams, StreamMessage};
use crate::connected_clients::{ConnectedClient, Connections};
use crate::port_allocator::PortAllocator;
use crate::remote;
use crate::{Config, ProxyServerError};

pub fn spawn<A: Into<SocketAddr>>(
    config: &'static OnceCell<Config>,
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
    remote_cancellers: Arc<DashMap<ClientId, CancellationToken>>,
    addr: A,
) -> JoinHandle<()> {
    let health_check = warp::get().and(warp::path("health_check")).map(|| {
        tracing::debug!("Health Check #2 triggered");
        "ok"
    });
    let client_conn = warp::path("tunnel").and(client_addr()).and(warp::ws()).map(
        move |client_addr: SocketAddr, ws: Ws| {
            let alloc_clone = alloc.clone();
            let remote_cancellers_clone = remote_cancellers.clone();
            ws.on_upgrade(move |w| {
                async move {
                    handle_new_connection(
                        config,
                        conn,
                        active_streams,
                        alloc_clone,
                        remote_cancellers_clone,
                        client_addr,
                        w,
                    )
                    .await
                }
                .instrument(tracing::info_span!("handle_websocket"))
            })
        },
    );

    let routes = client_conn.or(health_check);

    // TODO tls https://docs.rs/warp/0.3.1/warp/struct.Server.html#method.tls
    tokio::spawn(warp::serve(routes).run(addr.into()))
}

// fn client_ip() -> impl Filter<Extract = (IpAddr,), Error = Rejection> + Copy {
fn client_addr() -> impl Filter<Extract = (SocketAddr,), Error = Infallible> + Copy {
    warp::any()
        .and(warp::addr::remote())
        .map(|remote: Option<SocketAddr>| remote.unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0))))
}

pub struct ClientHandshake {
    pub id: ClientId,
    // pub sub_domain: String,
    // pub is_anonymous: bool,
    pub payload: Payload,
    pub port: u16,
}

#[tracing::instrument(skip(websocket, config, alloc))]
async fn try_client_handshake(
    websocket: &mut WebSocket,
    config: &'static OnceCell<Config>,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
) -> Option<ClientHandshake> {
    let host = match config.get() {
        Some(config) => {
            &config.host
        },
        None => {
            tracing::error!("failed to read config");
            return None;
        }
    };

    let client_hello_data = match read_client_hello(websocket).await {
        Some(client_hello_data) => {
            tracing::debug!("read client hello");
            client_hello_data
        },
        None => {
            // if client send nothing, server also send nothing
            return None;
        }
    };
    match verify_client_handshake(config, client_hello_data).await {
        Ok(payload) => {
            // TODO: initialization of StdRng may takes time
            let mut rng = StdRng::from_entropy();
            match alloc.lock().await.allocate_port(&mut rng) {
                Ok(port) => {
                    let client_id = ClientId::new();
                    let server_hello = ServerHello::Success {
                        client_id,
                        remote_addr: format!("{}:{}", host, port),
                    };

                    if let Err(e) = send_server_hello(websocket, server_hello).await {
                        tracing::warn!("failed to send server hello: {}", e);
                        return None
                    }

                    increment_counter!("magic_tunnel_server.control.handshake.success");
                    Some(ClientHandshake {
                        id: client_id,
                        port,
                        payload
                    })
                },
                Err(_) => {
                    tracing::error!("failed to allocate port");
                    let server_hello = ServerHello::ServiceTemporaryUnavailable;

                    if let Err(e) = send_server_hello(websocket, server_hello).await {
                        tracing::warn!("failed to send server hello: {}", e);
                    }
                    increment_counter!("magic_tunnel_server.control.handshake.error.service_temporary_unavailable");
                    None
                }
            }
        },
        Err(VerifyClientHandshakeError::InvalidClientHello) => {
            tracing::warn!("failed to verify client hello");
            let server_hello = ServerHello::BadRequest;

            if let Err(e) = send_server_hello(websocket, server_hello).await {
                tracing::warn!("failed to send server hello: {}", e);
            }
            increment_counter!("magic_tunnel_server.control.handshake.error.invalid_client_hello");
            None
        },
        Err(VerifyClientHandshakeError::InvalidJWT) => {
            tracing::warn!("client jwt has malformed");

            let server_hello = ServerHello::BadRequest;
            if let Err(e) = send_server_hello(websocket, server_hello).await {
                tracing::warn!("failed to send server hello: {}", e);
            }
            increment_counter!("magic_tunnel_server.control.handshake.error.invalid_jwt");
            None
        },
        Err(VerifyClientHandshakeError::IllegalHost) => {
            tracing::warn!("client try to connect to non-designated host");
            
            let server_hello = ServerHello::IllegalHost;
            if let Err(e) = send_server_hello(websocket, server_hello).await {
                tracing::warn!("failed to send server hello: {}", e);
            }
            increment_counter!("magic_tunnel_server.control.handshake.error.illegal_host");
            None
        },
        Err(VerifyClientHandshakeError::VersionMismatch) => {
            tracing::warn!("client sent not supported client handshake version");

            let server_hello = ServerHello::VersionMismatch;
            if let Err(e) = send_server_hello(websocket, server_hello).await {
                tracing::warn!("failed to send server hello: {}", e);
            }
            increment_counter!("magic_tunnel_server.control.handshake.error.version_mismatch");
            None
        }
        Err(VerifyClientHandshakeError::Other(e)) => {
            tracing::error!("proxy server encountered internal server error: {:?}", e);
            let server_hello = ServerHello::InternalServerError;
            if let Err(e) = send_server_hello(websocket, server_hello).await {
                tracing::warn!("failed to send server hello: {}", e);
            }
            increment_counter!("magic_tunnel_server.control.handshake.error.other");
            None
        }
    }
}

#[derive(Error, Debug, PartialEq)]
pub enum VerifyClientHandshakeError {
    #[error("Failed to deserialize client hello.")]
    InvalidClientHello,

    #[error("Failed to parse client jwt.")]
    InvalidJWT,

    #[error("Client tried to access different host.")]
    IllegalHost,

    #[error("Client sends unsupported client handshake version.")]
    VersionMismatch,

    #[error("Other internal error: {0}.")]
    Other(#[from] ProxyServerError),
}

#[tracing::instrument(skip(config))]
async fn verify_client_handshake(
    config: &'static OnceCell<Config>,
    client_hello_data: Vec<u8>,
) -> Result<Payload, VerifyClientHandshakeError> {
    let (token_secret, host) = match config.get() {
        Some(config) => (&config.token_secret, &config.host),
        None => {
            tracing::error!("failed to read config");
            return Err(VerifyClientHandshakeError::Other(ProxyServerError::ConfigNotInitialized));
        }
    };

    let client_hello: ClientHello = match serde_json::from_slice(&client_hello_data) {
        Ok(client_hello) => client_hello,
        _ => {
            tracing::error!("failed to deserialize client hello");
            return Err(VerifyClientHandshakeError::InvalidClientHello);
        }
    };
    tracing::debug!("got client handshake {:?}", client_hello);

    if client_hello.version != CLIENT_HELLO_VERSION {
        tracing::debug!("client sernt client hello version {} but server accept version {}", client_hello.version, CLIENT_HELLO_VERSION);
        return Err(VerifyClientHandshakeError::VersionMismatch);
    }

    let claim = decode_jwt(token_secret, &client_hello.token);
    match claim.map(|c| &c.host == host) {
        Ok(true) => {
            tracing::info!("successfully validate client jwt");
        },
        Ok(false) => {
            tracing::info!("client jwt was valid but different host");
            return Err(VerifyClientHandshakeError::IllegalHost);
        },
        Err(e) => {
            tracing::info!("failed to parse client jwt: {:?}", e);
            return Err(VerifyClientHandshakeError::InvalidJWT);
        }
    };

    Ok(client_hello.payload)
}

#[tracing::instrument(skip(websocket))]
async fn read_client_hello(
    websocket: &mut (impl Unpin + Stream<Item = Result<Message, WarpError>>)
) -> Option<Vec<u8>> {
    let client_hello_data = match websocket.next().await {
        Some(Ok(msg)) if (msg.is_binary() || msg.is_text()) && !msg.as_bytes().is_empty() => {
            msg.into_bytes()
        }
        _ => {
            tracing::warn!("client did not send hello");
            return None
        }
    };

    Some(client_hello_data)
}

#[tracing::instrument(skip(websocket))]
async fn send_server_hello<T>(websocket: &mut T, server_hello: ServerHello) -> Result<(), T::Error>
where
    T: Unpin + Sink<Message>,
{
    tracing::debug!("send server handshake {:?}", server_hello);
    let data = serde_json::to_vec(&server_hello).unwrap_or_default();

    websocket.send(Message::binary(data.clone())).await?;

    Ok(())
}

#[tracing::instrument(skip(config, conn, active_streams, alloc, remote_cancellers, websocket))]
async fn handle_new_connection(
    config: &'static OnceCell<Config>,
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
    remote_cancellers: Arc<DashMap<ClientId, CancellationToken>>,
    client_ip: SocketAddr,
    mut websocket: WebSocket,
) {
    increment_counter!("magic_tunnel_server.control.connections.success");
    let handshake = match try_client_handshake(&mut websocket, config, alloc).await {
        Some(ws) => ws,
        None => return,
    };
    let client_id = handshake.id;
    tracing::info!(cid = %client_id, port = %handshake.port, "open tunnel");
    let host = format!("host-foobar-{}", handshake.port);
    // let listen_addr = format!("[::]:{}", handshake.port); // 今までは v6 で listen すると v4 でも listen していた？
    let listen_addr = format!("0.0.0.0:{}", handshake.port);
    let canceller =
        match handshake.payload {
            Payload::UDP => {
                match remote::udp::spawn_remote(conn, active_streams, listen_addr, host.clone()).await {
                    Ok(canceller) => canceller,
                    Err(_) => {
                        tracing::error!("failed to bind to allocated port");
                        return;
                    }
                }
            }
            _ => {
                match remote::tcp::spawn_remote(conn, active_streams, listen_addr, host.clone()).await {
                    Ok(canceller) => canceller,
                    Err(_) => {
                        tracing::error!("failed to bind to allocated port");
                        return;
                    }
                }
            }
        };

    remote_cancellers.insert(client_id, canceller);
    tracing::debug!(cid = %client_id, "register remote_cancellers len={}", remote_cancellers.len());

    let (tx, rx) = unbounded::<ControlPacket>();
    let client = ConnectedClient::new(client_id, host, tx);
    Connections::add(conn, client.clone());
    tracing::debug!(cid = %client_id, "register client to connections len_clients={} len_hosts={}", Connections::len_clients(conn), Connections::len_hosts(conn));
    let active_streams = active_streams.clone();
    let (sink, stream) = websocket.split();

    let client_clone = client.clone();
    let remote_cancellers_clone = remote_cancellers.clone();
    let client_id_clone = client_id;
    tokio::spawn(
        async move {
            let client = client_clone;
            let remote_cancellers = remote_cancellers_clone;
            let client_id = client_id_clone;

            let client = tunnel_client(client, sink, rx).await;
            Connections::remove(conn, &client);
            tracing::debug!(cid = %client_id, "remove client from connections len_clients={} len_hosts={}", Connections::len_clients(conn), Connections::len_hosts(conn));
            if let Some((_cid, ct)) = remote_cancellers.remove(&client_id) {
                tracing::debug!(cid = %client_id, "cancel remote process");
                ct.cancel();
            }
        }
        .instrument(tracing::info_span!("tunnel_client")),
    );
    tokio::spawn(
        async move {
            let client = process_client_messages(active_streams, client, stream).await;
            Connections::remove(conn, &client);
            tracing::debug!(cid = %client_id, "remove client from connections len_clients={} len_hosts={}", Connections::len_clients(conn), Connections::len_hosts(conn));
            if let Some((_cid, ct)) = remote_cancellers.remove(&client_id) {
                tracing::debug!(cid = %client_id, "cancel remote process");
                ct.cancel();
            }
        }
        .instrument(tracing::info_span!("process_client")),
    );
}

/// Send the client a "stream init" message
pub async fn send_client_stream_init(mut stream: ActiveStream) -> Result<(), SendError> {
    stream.client.send_to_client(ControlPacket::Init(stream.id)).await
}

/// Process client control messages
#[tracing::instrument(skip(client_conn, active_streams, client))]
pub async fn process_client_messages<T>(
    active_streams: ActiveStreams,
    client: ConnectedClient,
    mut client_conn: T,
) -> ConnectedClient
where
    T: Stream<Item = Result<Message, WarpError>> + Unpin,
{
    loop {
        let result = client_conn.next().await;

        let message = match result {
            // handle protocol message
            Some(Ok(msg)) if (msg.is_binary() || msg.is_text()) && !msg.as_bytes().is_empty() => {
                msg.into_bytes()
            }
            // handle close with reason
            Some(Ok(msg)) if msg.is_close() && !msg.as_bytes().is_empty() => {
                tracing::debug!(close_reason=?msg, "got close");
                return client;
            }
            _ => {
                tracing::debug!(cid = %client.id, "goodbye client");
                return client;
            }
        };

        let packet: ControlPacket = match rmp_serde::from_slice(&message) {
            Ok(packet) => packet,
            Err(e) => {
                tracing::error!(error = ?e, "invalid data packet");
                continue;
            }
        };

        tracing::trace!(cid = %client.id, ?packet, "got control packet from client");
        let (stream_id, message) = match packet {
            ControlPacket::Data(stream_id, data) => {
                tracing::trace!(cid = %client.id, sid = %stream_id, "forwarding to stream: {}", data.len());
                (stream_id, StreamMessage::Data(data))
            }
            ControlPacket::Refused(stream_id) => {
                tracing::debug!(cid = %client.id, sid = %stream_id, "tunnel says: refused");
                (stream_id, StreamMessage::TunnelRefused)
            }
            ControlPacket::Init(stream_id) | ControlPacket::End(stream_id) => {
                tracing::error!(cid = %client.id, sid = %stream_id, "invalid protocol control::init message");
                continue;
            }
            ControlPacket::Ping => {
                tracing::trace!(cid = %client.id, "pong");
                // Connections::add(connections, client.clone());
                continue;
            }
            ControlPacket::UdpData(stream_id, data) => {
                tracing::trace!(cid = %client.id, sid = %stream_id, "forwarding udp to stream: {}", data.len());
                (stream_id, StreamMessage::Data(data))
            }
        };

        let stream = active_streams.get(&stream_id).map(|s| s.value().clone());

        if let Some(mut stream) = stream {
            tracing::trace!(cid = %client.id, sid = %stream_id, "forward message to active stream");
            let _ = stream.tx.send(message).await.map_err(|error| {
                tracing::debug!(cid = %client.id, sid = %stream_id, error = ?error, "Failed to send to stream tx");
            });
        }
    }
}

// async fn tunnel_client(
//     client: ConnectedClient,
//     mut sink: SplitSink<WebSocket, Message>,
//     mut queue: UnboundedReceiver<ControlPacket>,
// ) -> ConnectedClient
#[must_use]
#[tracing::instrument(skip(sink, queue, client))]
pub async fn tunnel_client<T, U>(
    client: ConnectedClient,
    mut sink: T,
    mut queue: U,
) -> ConnectedClient
where
    T: Sink<Message> + Unpin,
    U: Stream<Item = ControlPacket> + Unpin,
    T::Error: std::fmt::Debug,
{
    loop {
        match queue.next().await {
            Some(packet) => {
                let data = match rmp_serde::to_vec(&packet) {
                    Ok(data) => data,
                    Err(error) => {
                        tracing::warn!(cid = %client.id, error = ?error, "failed to encode message");
                        return client;
                    }
                };

                let result = sink.send(Message::binary(data)).await;
                if let Err(error) = result {
                    tracing::debug!(cid = %client.id, error = ?error, "client disconnected: aborting");
                    return client;
                }
            }
            None => {
                tracing::debug!(cid = %client.id, "ending client tunnel");
                return client;
            }
        };
    }
}

#[cfg(test)]
mod verify_client_handshake_test {
    use super::*;
    use futures::channel::mpsc;
    use magic_tunnel_auth::make_jwt;
    use chrono::Duration;
    use magic_tunnel_lib::Payload;

    static CONFIG: OnceCell<Config> = OnceCell::new();
    static EMPTY_CONFIG: OnceCell<Config> = OnceCell::new();

    fn get_config() -> &'static OnceCell<Config> {
        CONFIG.get_or_init(||
            Config {
                control_port: 5000,
                token_secret: "supersecret".to_string(),
                host: "foohost.test.local".to_string(),
                remote_port_start: 10010,
                remote_port_end: 10011,
            }
        );
        &CONFIG
    }

    #[tokio::test]
    async fn accept_client_hello() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&ClientHello {
            version: CLIENT_HELLO_VERSION,
            token: make_jwt("supersecret", Duration::minutes(10), "foohost.test.local".to_string())?,
            payload: Payload::Other,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let hello = verify_client_handshake(config, client_hello_data).await;
        assert!(hello.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_text_hello() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let client_hello_data = Message::text("foobarbaz".to_string()).into_bytes();
        let handshake= verify_client_handshake(config, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::InvalidClientHello);
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_serialized_hello() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&"malformed".to_string()).unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let handshake= verify_client_handshake(config, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::InvalidClientHello);
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_jwt() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&ClientHello {
            version: CLIENT_HELLO_VERSION,
            token: "invalid jwt".to_string(),
            payload: Payload::Other,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let handshake= verify_client_handshake(config, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::InvalidJWT);
        Ok(())
    }

    #[tokio::test]
    async fn reject_illegal_host() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&ClientHello {
            version: CLIENT_HELLO_VERSION,
            token: make_jwt("supersecret", Duration::minutes(10), "other.host.test.local".to_string())?,
            payload: Payload::Other,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let handshake= verify_client_handshake(config, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::IllegalHost);
        Ok(())
    }

    #[tokio::test]
    async fn reject_when_config_not_initialized() -> Result<(), Box<dyn std::error::Error>> {
        let hello = serde_json::to_vec(&ClientHello {
            version: CLIENT_HELLO_VERSION,
            token: make_jwt("supersecret", Duration::minutes(10), "foohost.test.local".to_string())?,
            payload: Payload::Other,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let handshake= verify_client_handshake(&EMPTY_CONFIG, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::Other(ProxyServerError::ConfigNotInitialized));
        Ok(())
    }

    #[tokio::test]
    async fn reject_when_version_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&ClientHello {
            version: CLIENT_HELLO_VERSION,
            token: make_jwt("supersecret", Duration::minutes(10), "foohost.test.local".to_string())?,
            payload: Payload::Other,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let hello = verify_client_handshake(config, client_hello_data).await;
        assert!(hello.is_ok());
        Ok(())
    }
}

#[cfg(test)]
mod process_client_messages_test {
    use super::*;
    use dashmap::DashMap;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use std::{sync::Arc, str::FromStr};

    async fn send_messages_to_client_and_process_client_message(
        is_add_stream_to_streams: bool,
        messages: Vec<Box<dyn Fn(StreamId) -> Message>>,
    ) -> Result<UnboundedReceiver<StreamMessage>, Box<dyn std::error::Error>> {
        let (mut stream_tx, stream_rx) = unbounded::<Result<Message, WarpError>>();
        let active_streams = ActiveStreams::default();

        let (tx, _rx) = unbounded::<ControlPacket>();
        let client = ConnectedClient::new(ClientId::new(), "foobar".into(), tx);
        let addr = "127.0.0.1:12345".parse().unwrap();

        let (active_stream, queue_rx) = ActiveStream::new(client.clone());
        let stream_id = active_stream.id;
        if is_add_stream_to_streams {
            active_streams.insert(stream_id, active_stream.clone(), addr);
        }

        for message in messages {
            let msg = message(stream_id);
            stream_tx.send(Ok(msg)).await?;
        }
        stream_tx.close_channel();

        let _ = process_client_messages(active_streams, client, stream_rx).await;
        // all active stream must be dropped
        // so that queue_rx.next().await returns None
        drop(active_stream);

        Ok(queue_rx)
    }

    #[tokio::test]
    async fn discard_control_packet_data_no_active_stream() -> Result<(), Box<dyn std::error::Error>>
    {
        let message = |stream_id| {
            let packet = ControlPacket::Data(stream_id, b"foobarbaz".to_vec());
            let data = rmp_serde::to_vec(&packet).unwrap();
            Message::binary(data)
        };
        let mut queue_rx =
            send_messages_to_client_and_process_client_message(false, vec![Box::new(message)])
                .await?;
        assert_eq!(queue_rx.next().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn forward_control_packet_data_to_appropriate_stream(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let message = |stream_id| {
            let packet = ControlPacket::Data(stream_id, b"foobarbaz".to_vec());
            let data = rmp_serde::to_vec(&packet).unwrap();
            Message::binary(data)
        };
        let mut queue_rx =
            send_messages_to_client_and_process_client_message(true, vec![Box::new(message)])
                .await?;

        // ControlPacket::Data must be sent to ActiveStream
        assert_eq!(
            queue_rx.next().await,
            Some(StreamMessage::Data(b"foobarbaz".to_vec()))
        );
        assert_eq!(queue_rx.next().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn forward_control_packet_refused_to_appropriate_stream(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let message = |stream_id| {
            let packet = ControlPacket::Refused(stream_id);
            let data = rmp_serde::to_vec(&packet).unwrap();
            Message::binary(data)
        };
        let mut queue_rx =
            send_messages_to_client_and_process_client_message(true, vec![Box::new(message)])
                .await?;

        // ControlPacket::Data must be sent to ActiveStream
        assert_eq!(queue_rx.next().await, Some(StreamMessage::TunnelRefused));
        assert_eq!(queue_rx.next().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn close_stream_remove_client() -> Result<(), Box<dyn std::error::Error>> {
        let message = |_| Message::close();
        let mut queue_rx =
            send_messages_to_client_and_process_client_message(true, vec![Box::new(message)])
                .await?;

        assert_eq!(queue_rx.next().await, None);
        Ok(())
    }
}

#[cfg(test)]
mod tunnel_client_test {
    use super::*;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};

    async fn send_control_packet_and_forward_to_websocket(
        packets: Vec<Box<dyn Fn(StreamId) -> ControlPacket>>,
    ) -> Result<(StreamId, UnboundedReceiver<Message>), Box<dyn std::error::Error>> {
        let (tx, _rx) = unbounded::<ControlPacket>();
        let client_id = ClientId::new();
        let client = ConnectedClient::new(client_id, "foobar".into(), tx);

        let (ws_tx, ws_rx) = unbounded::<Message>();
        let (mut control_tx, control_rx) = unbounded::<ControlPacket>();
        let stream_id = StreamId::new();

        for packet in packets {
            let pkt = packet(stream_id);
            control_tx.send(pkt).await?;
        }
        control_tx.close_channel();

        let _ = tunnel_client(client, ws_tx, control_rx).await;

        Ok((stream_id, ws_rx))
    }

    #[tokio::test]
    async fn forward_control_packet_to_websocket() -> Result<(), Box<dyn std::error::Error>> {
        let packet = |stream_id| ControlPacket::Init(stream_id);
        let (stream_id, mut ws_rx) =
            send_control_packet_and_forward_to_websocket(vec![Box::new(packet)]).await?;

        let payload = ws_rx.next().await.unwrap().into_bytes();

        let packet: ControlPacket = rmp_serde::from_slice(&payload)?;
        assert_eq!(packet, ControlPacket::Init(stream_id));
        assert_eq!(ws_rx.next().await, None);

        Ok(())
    }
}
