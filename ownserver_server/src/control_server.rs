use futures::{
    Sink, SinkExt, Stream, StreamExt,
};
use ownserver_lib::Payload;
pub use ownserver_lib::{ClientHello, ClientId, ControlPacket, ServerHello, StreamId, CLIENT_HELLO_VERSION};
use ownserver_auth::decode_jwt;
use metrics::increment_counter;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::task::JoinHandle;
use tracing::Instrument;
use warp::{
    ws::{Message, WebSocket, Ws},
    Error as WarpError, Filter,
};

use rand::{rngs::StdRng, SeedableRng};
use std::ops::Range;
use std::sync::Arc;
use tokio::sync::Mutex;
use once_cell::sync::OnceCell;
use thiserror::Error;

use crate::{Store, Client};
use crate::port_allocator::PortAllocator;
use crate::remote;
use crate::{Config, ProxyServerError};

#[tracing::instrument(skip(config, store, alloc))]
pub fn spawn<A: Into<SocketAddr> + std::fmt::Debug>(
    config: &'static OnceCell<Config>,
    store: Arc<Store>,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
    addr: A,
) -> JoinHandle<()> {
    let health_check = warp::get().and(warp::path("health_check")).map(|| {
        tracing::debug!("Health Check #2 triggered");
        "ok"
    });

    let client_conn = warp::path("tunnel").and(client_addr()).and(warp::ws()).map(
        move |client_addr: SocketAddr, ws: Ws| {
            let alloc_ = alloc.clone();
            let store_ = store.clone();
            ws.on_upgrade(move |w| {
                async move {
                    handle_new_connection(
                        config,
                        store_,
                        alloc_,
                        client_addr,
                        w
                    ).await
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
        .map(|remote: Option<SocketAddr>| remote.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0))))
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

                    increment_counter!("ownserver_server.control_server.try_client_handshake.success");
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
                    increment_counter!("ownserver_server.control_server.try_client_handshake.service_temporary_unavailable");
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
            increment_counter!("ownserver_server.control_server.try_client_handshake.invalid_client_hello");
            None
        },
        Err(VerifyClientHandshakeError::InvalidJWT) => {
            tracing::warn!("client jwt has malformed");

            let server_hello = ServerHello::BadRequest;
            if let Err(e) = send_server_hello(websocket, server_hello).await {
                tracing::warn!("failed to send server hello: {}", e);
            }
            increment_counter!("ownserver_server.control_server.try_client_handshake.invalid_jwt");
            None
        },
        Err(VerifyClientHandshakeError::IllegalHost) => {
            tracing::warn!("client try to connect to non-designated host");
            
            let server_hello = ServerHello::IllegalHost;
            if let Err(e) = send_server_hello(websocket, server_hello).await {
                tracing::warn!("failed to send server hello: {}", e);
            }
            increment_counter!("ownserver_server.control_server.try_client_handshake.illegal_host");
            None
        },
        Err(VerifyClientHandshakeError::VersionMismatch) => {
            tracing::warn!("client sent not supported client handshake version");

            let server_hello = ServerHello::VersionMismatch;
            if let Err(e) = send_server_hello(websocket, server_hello).await {
                tracing::warn!("failed to send server hello: {}", e);
            }
            increment_counter!("ownserver_server.control_server.try_client_handshake.version_mismatch");
            None
        }
        Err(VerifyClientHandshakeError::Other(e)) => {
            tracing::error!("proxy server encountered internal server error: {:?}", e);
            let server_hello = ServerHello::InternalServerError;
            if let Err(e) = send_server_hello(websocket, server_hello).await {
                tracing::warn!("failed to send server hello: {}", e);
            }
            increment_counter!("ownserver_server.control_server.try_client_handshake.other");
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

#[tracing::instrument(skip(config, store, alloc, websocket))]
async fn handle_new_connection(
    config: &'static OnceCell<Config>,
    store: Arc<Store>,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
    client_ip: SocketAddr,
    mut websocket: WebSocket,
) {
    increment_counter!("ownserver_server.control_server.handle_new_connection");
    let handshake = match try_client_handshake(&mut websocket, config, alloc).await {
        Some(ws) => ws,
        None => return,
    };
    let client_id = handshake.id;
    tracing::info!(cid = %client_id, port = %handshake.port, "open tunnel");
    let listen_addr = format!("0.0.0.0:{}", handshake.port);


    let client = Client::new(store.clone(), client_id, handshake.port, websocket);
    let ct = client.cancellation_token();
    store.add_client(client).await;
    tracing::info!(cid=%client_id, "register client to store");

    match handshake.payload {
        Payload::UDP => {
            if let Err(e) = remote::udp::spawn_remote(store.clone(), listen_addr, client_id, ct).await {
                tracing::error!(cid = %client_id, port = %handshake.port, "failed to spawn remote listener {:?}", e);
            }
        }
        _ => {
            if let Err(e) = remote::tcp::spawn_remote(store.clone(), listen_addr, client_id, ct).await {
                tracing::error!(cid = %client_id, port = %handshake.port, "failed to spawn remote listener {:?}", e);
            }
        }
    }
}

#[cfg(test)]
mod verify_client_handshake_test {
    use super::*;
    use ownserver_auth::make_jwt;
    use chrono::Duration;
    use ownserver_lib::Payload;

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