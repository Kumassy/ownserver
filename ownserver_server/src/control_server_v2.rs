use chrono::Utc;
use futures::{
    Sink, SinkExt, Stream, StreamExt,
};
use ownserver_lib::{ClientHelloV2, ClientType, ControlPacketV2, EndpointClaim, EndpointClaims, Protocol, ServerHelloV2};
pub use ownserver_lib::{ClientId, StreamId, CLIENT_HELLO_VERSION};
use ownserver_auth::decode_jwt;
use metrics::increment_counter;
use std::{convert::Infallible, time::Duration};
use std::net::SocketAddr;
use tokio::time::sleep;
use tokio::task::JoinSet;
use tracing::Instrument;
use warp::{
    ws::{Message, WebSocket, Ws},
    Error as WarpError, Filter,
};

use rand::{rngs::StdRng, SeedableRng};
use std::sync::Arc;
use once_cell::sync::OnceCell;
use thiserror::Error;

use crate::{Store, Client};
use crate::remote;
use crate::Config;

#[tracing::instrument(skip(config, store))]
pub fn spawn<A: Into<SocketAddr> + std::fmt::Debug>(
    config: &'static OnceCell<Config>,
    store: Arc<Store>,
    addr: A,
) -> JoinSet<()> {
    let periodic_cleanup_interval = config.get().expect("failed to read config").periodic_cleanup_interval;
    let periodic_ping_interval = config.get().expect("failed to read config").periodic_ping_interval;

    let health_check = warp::get().and(warp::path("health_check")).map(|| {
        tracing::debug!("Health Check #2 triggered");
        "ok"
    });

    let store_ = store.clone();
    let client_conn = warp::path("tunnel").and(client_addr()).and(warp::ws()).map(
        move |client_addr: SocketAddr, ws: Ws| {
            let store_ = store_.clone();
            ws.on_upgrade(move |w| {
                async move {
                    handle_new_connection(
                        config,
                        store_,
                        client_addr,
                        w
                    ).await
                }
                .instrument(tracing::info_span!("handle_websocket"))
            })
        },
    );

    let routes = client_conn.or(health_check);

    let mut set = JoinSet::new();
    // TODO tls https://docs.rs/warp/0.3.1/warp/struct.Server.html#method.tls
    set.spawn(warp::serve(routes).run(addr.into()));

    let store_ = store.clone();
    set.spawn(async move {
        loop {
            sleep(Duration::from_secs(periodic_cleanup_interval)).await;
            store_.cleanup().await;
        }
    });

    set.spawn(async move {
        loop {
            sleep(Duration::from_secs(periodic_ping_interval)).await;

            let current_time = Utc::now();
            store.broadcast_to_clients(ControlPacketV2::Ping(0, current_time)).await;
            tracing::debug!("broadcasted ping");
        }
    });
    
    set
}

// fn client_ip() -> impl Filter<Extract = (IpAddr,), Error = Rejection> + Copy {
fn client_addr() -> impl Filter<Extract = (SocketAddr,), Error = Infallible> + Copy {
    warp::any()
        .and(warp::addr::remote())
        .map(|remote: Option<SocketAddr>| remote.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0))))
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
async fn send_server_hello<T>(websocket: &mut T, server_hello: &ServerHelloV2) -> Result<(), T::Error>
where
    T: Unpin + Sink<Message>,
{
    tracing::debug!("send server handshake {:?}", server_hello);
    let data = serde_json::to_vec(&server_hello).unwrap_or_default();

    websocket.send(Message::binary(data.clone())).await?;

    Ok(())
}

#[tracing::instrument(skip(config))]
async fn validate_client_hello(
    config: &'static OnceCell<Config>,
    client_hello_data: Vec<u8>,
) -> Result<Vec<EndpointClaim>, VerifyClientHandshakeError> {
    let Config { ref token_secret, ref host, .. } = config.get().expect("failed to read config");

    let client_hello: ClientHelloV2 = match serde_json::from_slice(&client_hello_data) {
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

    match client_hello.client_type {
        ClientType::Auth => {
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
        
            Ok(client_hello.endpoint_claims)
        },
        _ => {
            unimplemented!()
        }
    }
}


async fn process_client_claims(
    config: &'static OnceCell<Config>,
    store: Arc<Store>,
    client_hello: Result<EndpointClaims, VerifyClientHandshakeError>,
) -> ServerHelloV2 {
    let Config { ref host, .. } = config.get().expect("failed to read config");
    let mut rng = StdRng::from_entropy();
    match client_hello {
        Ok(claims) => {
            match store.allocate_endpoints(&mut rng, claims).await {
                Ok(endpoints) => {
                    let server_hello = ServerHelloV2::Success {
                        client_id: ClientId::new(),
                        host: host.to_string(),
                        endpoints,
                    };

                    increment_counter!("ownserver_server.control_server.process_client_claims.success");
                    server_hello
                },
                Err(_) => {
                    tracing::error!("failed to allocate port");
                    increment_counter!("ownserver_server.control_server.process_client_claims.service_temporary_unavailable");

                    ServerHelloV2::ServiceTemporaryUnavailable
                }
            }
        }
        Err(VerifyClientHandshakeError::InvalidClientHello) => {
            tracing::warn!("failed to verify client hello");
            increment_counter!("ownserver_server.control_server.process_client_claims.invalid_client_hello");

            ServerHelloV2::BadRequest
        },
        Err(VerifyClientHandshakeError::InvalidJWT) => {
            tracing::warn!("client jwt has malformed");
            increment_counter!("ownserver_server.control_server.process_client_claims.invalid_jwt");

            ServerHelloV2::BadRequest
        },
        Err(VerifyClientHandshakeError::IllegalHost) => {
            tracing::warn!("client try to connect to non-designated host");
            increment_counter!("ownserver_server.control_server.process_client_claims.illegal_host");
            
            ServerHelloV2::IllegalHost
        },
        Err(VerifyClientHandshakeError::VersionMismatch) => {
            tracing::warn!("client sent not supported client handshake version");
            increment_counter!("ownserver_server.control_server.process_client_claims.version_mismatch");

            ServerHelloV2::VersionMismatch
        }
    }
}




#[tracing::instrument(skip(config, store, websocket))]
async fn handle_new_connection(
    config: &'static OnceCell<Config>,
    store: Arc<Store>,
    client_ip: SocketAddr,
    mut websocket: WebSocket,
) {
    increment_counter!("ownserver_server.control_server.handle_new_connection");


    // 1. read client hello
    let client_hello_data = match read_client_hello(&mut websocket).await {
        Some(data) => data,
        None => {
            increment_counter!("ownserver_server.control_server.handle_new_connection.read_client_hello_error");
            return
        }
    };

    // 2. parse and validate client hello
    let client_hello = validate_client_hello(config, client_hello_data).await;

    // 3. convert client hello to server hello
    // allocate ports based on client claims
    let server_hello = process_client_claims(config, store.clone(), client_hello).await;

    // 4. respond with server hello
    if let Err(e) = send_server_hello(&mut websocket, &server_hello).await {
        tracing::error!("failed to send server hello: {:?}", e);
        increment_counter!("ownserver_server.control_server.handle_new_connection.send_server_hello_error");
        return;
    }

    let (client_id, endpoints) = match server_hello {
        ServerHelloV2::Success { client_id, endpoints, .. } => (client_id, endpoints),
        _ => {
            return;
        }
    };

    // 5. spawn remote listener
    let client = Client::new(store.clone(), client_id, endpoints.clone(), websocket);
    let ct = client.cancellation_token();
    store.add_client(client).await;
    tracing::info!(cid=%client_id, "register client to store");

    for endpoint in endpoints {
        match endpoint.protocol {
            Protocol::TCP => {
                if let Err(e) = remote::tcp::spawn_remote(store.clone(), client_id, endpoint.id, ct.clone()).await {
                    tracing::error!(cid = %client_id, eid = %endpoint.id, "failed to spawn remote listener {:?}", e);
                }
            }
            Protocol::UDP => {
                if let Err(e) = remote::udp::spawn_remote(store.clone(), client_id, endpoint.id, ct.clone()).await {
                    tracing::error!(cid = %client_id, eid = %endpoint.id, "failed to spawn remote listener {:?}", e);
                }
            }
        }

    }
}

#[cfg(test)]
mod verify_client_handshake_test {
    use super::*;
    use ownserver_auth::make_jwt;
    use chrono::Duration;
    use ownserver_lib::{ClientType, Protocol};

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
                periodic_cleanup_interval: 15,
                periodic_ping_interval: 15,
            }
        );
        &CONFIG
    }

    #[tokio::test]
    async fn accept_client_hello() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&ClientHelloV2 {
            version: CLIENT_HELLO_VERSION,
            token: make_jwt("supersecret", Duration::minutes(10), "foohost.test.local".to_string())?,
            endpoint_claims: vec![EndpointClaim {
                protocol: Protocol::TCP,
                local_port: 25565,
                remote_port: 0,
            }],
            client_type: ClientType::Auth,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let hello = validate_client_hello(config, client_hello_data).await;
        assert!(hello.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_text_hello() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let client_hello_data = Message::text("foobarbaz".to_string()).into_bytes();
        let handshake= validate_client_hello(config, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::InvalidClientHello);
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_serialized_hello() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&"malformed".to_string()).unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let handshake= validate_client_hello(config, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::InvalidClientHello);
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_jwt() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&ClientHelloV2 {
            version: CLIENT_HELLO_VERSION,
            token: "invalid jwt".to_string(),
            endpoint_claims: vec![EndpointClaim {
                protocol: Protocol::TCP,
                local_port: 25565,
                remote_port: 0,
            }],
            client_type: ClientType::Auth,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let handshake= validate_client_hello(config, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::InvalidJWT);
        Ok(())
    }

    #[tokio::test]
    async fn reject_illegal_host() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&ClientHelloV2 {
            version: CLIENT_HELLO_VERSION,
            token: make_jwt("supersecret", Duration::minutes(10), "other.host.test.local".to_string())?,
            endpoint_claims: vec![EndpointClaim {
                protocol: Protocol::TCP,
                local_port: 25565,
                remote_port: 0,
            }],
            client_type: ClientType::Auth,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let handshake= validate_client_hello(config, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::IllegalHost);
        Ok(())
    }

    #[tokio::test]
    #[should_panic]
    async fn panic_when_config_not_initialized() {
        let hello = serde_json::to_vec(&ClientHelloV2 {
            version: CLIENT_HELLO_VERSION,
            token: make_jwt("supersecret", Duration::minutes(10), "foohost.test.local".to_string()).unwrap(),
            endpoint_claims: vec![EndpointClaim {
                protocol: Protocol::TCP,
                local_port: 25565,
                remote_port: 0,
            }],
            client_type: ClientType::Auth,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let _ = validate_client_hello(&EMPTY_CONFIG, client_hello_data).await;
    }

    #[tokio::test]
    async fn reject_when_version_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let config = get_config();

        let hello = serde_json::to_vec(&ClientHelloV2 {
            version: CLIENT_HELLO_VERSION,
            token: make_jwt("supersecret", Duration::minutes(10), "foohost.test.local".to_string())?,
            endpoint_claims: vec![EndpointClaim {
                protocol: Protocol::TCP,
                local_port: 25565,
                remote_port: 0,
            }],
            client_type: ClientType::Auth,
        })
        .unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let hello = validate_client_hello(config, client_hello_data).await;
        assert!(hello.is_ok());
        Ok(())
    }
}