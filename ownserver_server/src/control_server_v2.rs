use chrono::Utc;
use futures::{
    Sink, SinkExt, Stream, StreamExt,
};
use metrics::counter;
use ownserver_lib::reconnect::ReconnectTokenPayload;
use ownserver_lib::{ClientHelloV2, ClientType, ControlPacketV2, EndpointClaims, Protocol, ServerHelloV2};
pub use ownserver_lib::{ClientId, StreamId, CLIENT_HELLO_VERSION};
use ownserver_auth::decode_jwt;
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
use thiserror::Error;

use crate::{Store, Client};
use crate::remote;
use crate::Config;

#[tracing::instrument(skip(config, store))]
pub fn spawn<A: Into<SocketAddr> + std::fmt::Debug>(
    config: &'static Config,
    store: Arc<Store<WebSocket>>,
    addr: A,
) -> JoinSet<()> {
    let periodic_cleanup_interval = config.periodic_cleanup_interval;
    let periodic_ping_interval = config.periodic_ping_interval;

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
            let client_ids = store.get_client_ids().await;

            for client_id in client_ids {
                tracing::debug!(cid = %client_id, "send ping to client");
                match ReconnectTokenPayload::new(client_id).into_token(&config.token_secret) {
                    Ok(token) => {
                        if let Err(e) = store.send_to_client(client_id, ControlPacketV2::Ping(0, current_time, Some(token))).await {
                            tracing::warn!(cid = %client_id, "failed to send reconnect token {:?}", e);
                        }
                    },
                    Err(e) => {
                        tracing::warn!(cid = %client_id, "failed to create reconnect token {:?}", e);
                    }
                }
            }
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

enum ProvisioningAction {
    NewClient {
        claims: EndpointClaims,
    },
    Reconnect {
        client_id: ClientId,
    }
}

#[tracing::instrument(skip(config))]
async fn validate_client_hello(
    config: &'static Config,
    client_hello_data: Vec<u8>,
) -> Result<ProvisioningAction, VerifyClientHandshakeError> {
    let Config { ref token_secret, ref host, .. } = config;

    let client_hello: ClientHelloV2 = match serde_json::from_slice(&client_hello_data) {
        Ok(client_hello) => client_hello,
        _ => {
            tracing::warn!("failed to deserialize client hello");
            counter!("ownserver_server.control_server.process_client_claims.invalid_client_hello").increment(1);

            return Err(VerifyClientHandshakeError::InvalidClientHello);
        }
    };
    tracing::debug!("got client handshake {:?}", client_hello);

    if client_hello.version != CLIENT_HELLO_VERSION {
        tracing::warn!("client sent not supported client handshake version. client: {}, server: {}", client_hello.version, CLIENT_HELLO_VERSION);
        counter!("ownserver_server.control_server.process_client_claims.version_mismatch").increment(1);

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
                    tracing::warn!("client try to connect to non-designated host");
                    counter!("ownserver_server.control_server.process_client_claims.illegal_host").increment(1);

                    return Err(VerifyClientHandshakeError::IllegalHost);
                },
                Err(e) => {
                    tracing::warn!("failed to parse client jwt: {:?}", e);
                    counter!("ownserver_server.control_server.process_client_claims.invalid_jwt").increment(1);

                    return Err(VerifyClientHandshakeError::InvalidJWT);
                }
            };
        
            Ok(ProvisioningAction::NewClient {
                claims: client_hello.endpoint_claims
            })
        },
        ClientType::Reconnect => {
            let payload = ReconnectTokenPayload::decode_token(token_secret, &client_hello.token).map_err(|e| {
                tracing::info!("failed to parse reconnect token: {:?}", e);
                VerifyClientHandshakeError::InvalidJWT
            })?;

            tracing::info!("successfully validate reconnect token");
            Ok(ProvisioningAction::Reconnect {
                client_id: payload.client_id
            })
        }
    }
}

impl From<VerifyClientHandshakeError> for ServerHelloV2 {
    fn from(e: VerifyClientHandshakeError) -> Self {
        match e {
            VerifyClientHandshakeError::InvalidClientHello => ServerHelloV2::BadRequest,
            VerifyClientHandshakeError::InvalidJWT => ServerHelloV2::BadRequest,
            VerifyClientHandshakeError::IllegalHost => ServerHelloV2::IllegalHost,
            VerifyClientHandshakeError::VersionMismatch => ServerHelloV2::VersionMismatch,
        }
    }
}

async fn process_client_claims(
    config: &'static Config,
    store: Arc<Store<WebSocket>>,
    claims: EndpointClaims
) -> ServerHelloV2 {
    let Config { ref host, .. } = config;
    let mut rng = StdRng::from_entropy();

    match store.allocate_endpoints(&mut rng, claims).await {
        Ok(endpoints) => {
            let server_hello = ServerHelloV2::Success {
                client_id: ClientId::new(),
                host: host.to_string(),
                endpoints,
            };

            counter!("ownserver_server.control_server.process_client_claims.success").increment(1);
            server_hello
        },
        Err(_) => {
            tracing::error!("failed to allocate port");
            counter!("ownserver_server.control_server.process_client_claims.service_temporary_unavailable").increment(1);

            ServerHelloV2::ServiceTemporaryUnavailable
        }
    }
}


#[tracing::instrument(skip(config, store, websocket))]
async fn handle_new_connection(
    config: &'static Config,
    store: Arc<Store<WebSocket>>,
    client_ip: SocketAddr,
    mut websocket: WebSocket,
) {
    counter!("ownserver_server.control_server.handle_new_connection").increment(1);


    // 1. read client hello
    let client_hello_data = match read_client_hello(&mut websocket).await {
        Some(data) => data,
        None => {
            counter!("ownserver_server.control_server.handle_new_connection.read_client_hello_error").increment(1);
            return
        }
    };

    // 2. parse and validate client hello
    let provisioning_action = match validate_client_hello(config, client_hello_data).await {
        Ok(action) => action,
        Err(e) => {
            let server_hello: ServerHelloV2 = e.into();
            if let Err(e) = send_server_hello(&mut websocket, &server_hello).await {
                tracing::error!("failed to send server hello: {:?}", e);
                counter!("ownserver_server.control_server.handle_new_connection.send_server_hello_error").increment(1);
            }
            return;
        },
    };

    match provisioning_action {
        ProvisioningAction::NewClient { claims } => {
            tracing::info!("process NewClient action");
            let Config { reconnect_window, ..  } = config;
            // 3. convert client hello to server hello
            // allocate ports based on client claims
            let server_hello = process_client_claims(config, store.clone(), claims).await;

            // 4. respond with server hello
            if let Err(e) = send_server_hello(&mut websocket, &server_hello).await {
                tracing::error!("failed to send server hello: {:?}", e);
                counter!("ownserver_server.control_server.handle_new_connection.send_server_hello_error").increment(1);
                return;
            }

            let (client_id, endpoints) = match server_hello {
                ServerHelloV2::Success { client_id, endpoints, .. } => (client_id, endpoints),
                _ => {
                    // Never happens
                    return;
                }
            };

            // 5. spawn remote listener
            let client = Client::new(store.clone(), client_id, endpoints.clone(), websocket, *reconnect_window);
            let ct_child = client.clone_child_token();
            store.add_client(client).await;
            tracing::info!(cid=%client_id, "register client to store");

            for endpoint in endpoints {
                match endpoint.protocol {
                    Protocol::TCP => {
                        if let Err(e) = remote::tcp::spawn_remote(store.clone(), client_id, endpoint.id, ct_child.clone()).await {
                            tracing::error!(cid = %client_id, eid = %endpoint.id, "failed to spawn remote listener {:?}", e);
                        }
                    }
                    Protocol::UDP => {
                        if let Err(e) = remote::udp::spawn_remote(store.clone(), client_id, endpoint.id, ct_child.clone()).await {
                            tracing::error!(cid = %client_id, eid = %endpoint.id, "failed to spawn remote listener {:?}", e);
                        }
                    }
                }

            }
            counter!("ownserver_server.control_server.handle_new_connection.newclient.success").increment(1);
        }
        ProvisioningAction::Reconnect { client_id } => {
            let Config { ref host, reconnect_window,  .. } = config;

            tracing::info!("process Reconnect action");
            let old_client = match store.remove_client(client_id).await {
                Some(client) => client,
                None => {
                    tracing::warn!(cid=%client_id, "try to reconnect, but client not found");
                    counter!("ownserver_server.control_server.handle_new_connection.reconnect.client_not_found").increment(1);
                    return;
                }
            };
            let endpoints = old_client.endpoints.clone();
            drop(old_client);

            let server_hello = ServerHelloV2::Success {
                client_id,
                host: host.clone(),
                endpoints: endpoints.clone(),
            };
            if let Err(e) = send_server_hello(&mut websocket, &server_hello).await {
                tracing::error!("failed to send server hello: {:?}", e);
                counter!("ownserver_server.control_server.handle_new_connection.send_server_hello_error").increment(1);
                return;
            }

            let new_client = Client::new(store.clone(), client_id, endpoints, websocket, *reconnect_window);
            store.add_client(new_client).await;
            tracing::info!(cid=%client_id, "register reconnect client to store");
            counter!("ownserver_server.control_server.handle_new_connection.reconnect.success").increment(1);
        },
    }

}

#[cfg(test)]
mod verify_client_handshake_test {
    use std::sync::LazyLock;

    use super::*;
    use ownserver_auth::make_jwt;
    use chrono::Duration;
    use ownserver_lib::{ClientType, EndpointClaim, Protocol};

    static CONFIG: LazyLock<Config> = LazyLock::new(||
        Config {
            control_port: 5000,
            token_secret: "supersecret".to_string(),
            host: "foohost.test.local".to_string(),
            metrics_idle_timeout: 300,
            remote_port_start: 10010,
            remote_port_end: 10011,
            periodic_cleanup_interval: 15,
            periodic_ping_interval: 15,
            reconnect_window: Duration::minutes(2)
        } 
    );

    #[tokio::test]
    async fn accept_client_hello() -> Result<(), Box<dyn std::error::Error>> {
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

        let hello = validate_client_hello(&CONFIG, client_hello_data).await;
        assert!(hello.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_text_hello() -> Result<(), Box<dyn std::error::Error>> {
        let client_hello_data = Message::text("foobarbaz".to_string()).into_bytes();
        let handshake= validate_client_hello(&CONFIG, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::InvalidClientHello);
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_serialized_hello() -> Result<(), Box<dyn std::error::Error>> {
        let hello = serde_json::to_vec(&"malformed".to_string()).unwrap_or_default();
        let client_hello_data = Message::binary(hello).into_bytes();

        let handshake= validate_client_hello(&CONFIG, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::InvalidClientHello);
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_jwt() -> Result<(), Box<dyn std::error::Error>> {
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

        let handshake= validate_client_hello(&CONFIG, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::InvalidJWT);
        Ok(())
    }

    #[tokio::test]
    async fn reject_illegal_host() -> Result<(), Box<dyn std::error::Error>> {
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

        let handshake= validate_client_hello(&CONFIG, client_hello_data).await;
        let handshake_error = handshake.err().unwrap();
        assert_eq!(handshake_error, VerifyClientHandshakeError::IllegalHost);
        Ok(())
    }

    #[tokio::test]
    async fn reject_when_version_mismatch() -> Result<(), Box<dyn std::error::Error>> {
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

        let hello = validate_client_hello(&CONFIG, client_hello_data).await;
        assert!(hello.is_ok());
        Ok(())
    }
}