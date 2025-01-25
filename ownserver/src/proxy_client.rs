use anyhow::{anyhow, Result};
use chrono::Utc;
use futures::{Sink, SinkExt, Stream, StreamExt};
use log::*;
use serde::{Deserialize, Serialize};
use tokio::signal;
use tokio::time::sleep;
use std::{cmp::min, sync::Arc};
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as WsError, Message},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::Error;
use crate::recorder::record_client_info;
use crate::{recorder::record_error, record_log, Client, Config, Store};
use ownserver_lib::{
    ClientHelloV2, ClientId, ClientType, ControlPacketV2, EndpointClaims, Endpoints, ServerHelloV2, CLIENT_HELLO_VERSION
};

#[derive(Debug, Clone)]
pub enum RequestType {
    NewClient {
        endpoint_claims: EndpointClaims,
    },
    Reconnect,
}

pub async fn run_with_token(
    store: Arc<Store>,
    control_port: u16,
    cancellation_token: CancellationToken,
    ping_interval: u64,
    token: String,
    host: String,
    request_type: RequestType,
) -> Result<(ClientInfo, JoinSet<Result<(), Error>>)> {
    println!("Connecting to proxy server: {}:{}", host, control_port);
    let url = Url::parse(&format!("ws://{}:{}/tunnel", host, control_port))?;
    let (mut websocket, _) = connect_async(url).await.map_err(|_| Error::ServerDown)?;
    info!("WebSocket handshake has been successfully completed");

    send_client_hello(&mut websocket, token.to_string(), request_type).await?;
    let client_info = verify_server_hello(&mut websocket).await?;
    info!(
        "cid={} got client_info from server: {:?}",
        client_info.client_id, client_info
    );
    println!("Your Client ID: {}", client_info.client_id);
    println!("Endpoint Info:");
    for endpoint in client_info.endpoints.iter() {
        let message = format!("{}://localhost:{} <--> {}://{}:{}", endpoint.protocol, endpoint.local_port, endpoint.protocol, client_info.host, endpoint.remote_port);
        println!("+{}+", "-".repeat(message.len() + 2));
        println!("| {} |", message);
        println!("+{}+", "-".repeat(message.len() + 2));
    }
    store.register_endpoints(client_info.endpoints.clone());

    let mut set = JoinSet::new();
    let client_id = client_info.client_id;
    let ct = cancellation_token.child_token();

    let client = Client::new(&mut set, store.clone(), client_info.clone(), websocket, ct.clone());
    store.add_client(client).await;

    set.spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(ping_interval)) => {
                    let now = Utc::now();
                    let packet = ControlPacketV2::Ping(0, now, None);

                    if let Err(e) = store.send_to_server(packet).await {
                        error!("cid={} failed to send ping to tx buffer: {:?}", &client_id, e);
                        return Ok(());
                    }
                },
                _ = ct.cancelled() => {
                    return Ok(());
                }
            }
        }
    });
    Ok((client_info, set))

}
pub async fn run(
    store: Arc<Store>,
    control_port: u16,
    token_server: &str,
    cancellation_token: CancellationToken,
    ping_interval: u64,
    request_type: RequestType,
) -> Result<(ClientInfo, JoinSet<Result<(), Error>>)> {
    println!("Connecting to auth server: {}", token_server);
    let (token, host) = fetch_token(token_server).await?;
    info!("got token: {}, host: {}", token, host);
    println!("Your proxy server: {}", host);

    run_with_token(store, control_port, cancellation_token, ping_interval, token, host, request_type).await
}

const MAX_RECONNECT_BACKOFF_SECS: u64 = 300;
fn calculate_reconnect_backoff(attempts: u32) -> Duration {
    match attempts {
        0 => Duration::from_secs(0),
        1..10 => Duration::from_secs(min(2u64.pow(attempts - 1), MAX_RECONNECT_BACKOFF_SECS)),
        _ => Duration::from_secs(MAX_RECONNECT_BACKOFF_SECS),
    }
}

pub async fn new_run_client(
    config: &'static Config,
    store: Arc<Store>,
    cancellation_token: CancellationToken,
    request_type: RequestType,
) -> Result<()> {
    // get token from token server
    record_log!("Connecting to auth server: {}", config.token_server);
    let (mut token, host) = fetch_token(&config.token_server).await?;
    info!("got token: {}, host: {}", token, host);
    record_log!("Your proxy server: {}", host);

    let mut reconnect_attempts = 0;
    let mut request_type = request_type;
    loop {
        let reconnect_backoff = calculate_reconnect_backoff(reconnect_attempts);
        record_log!("Connecting in {} seconds...", reconnect_backoff.as_secs());
        sleep(reconnect_backoff).await;

        // handshake
        record_log!("Connecting to proxy server: {}:{}", host, config.control_port);
        let url = Url::parse(&format!("ws://{}:{}/tunnel", host, config.control_port))?;

        let mut websocket = match connect_async(url).await {
            Ok((websocket, _)) => websocket,
            Err(_) => {
                record_error(Error::ServerDown);
                reconnect_attempts += 1;
                continue 
            }
        };
        info!("WebSocket handshake has been successfully completed");
    
        if let Err(e) = send_client_hello(&mut websocket, token.to_string(), request_type.clone()).await {
            record_error(e.into());
            reconnect_attempts += 1;
            continue;
        }

        let client_info = match verify_server_hello(&mut websocket).await {
            Ok(client_info) => client_info,
            Err(e) => {
                record_error(e);
                reconnect_attempts += 1;
                continue  
            }
        };
        let client_id = client_info.client_id;
        store.register_endpoints(client_info.endpoints.clone());
        record_client_info(client_info.clone());
        reconnect_attempts = 0;


        // spawn main thread
        let mut set = JoinSet::new();
        let ct = cancellation_token.child_token();
    
        let client = Client::new(&mut set, store.clone(), client_info, websocket, ct.clone());
        store.add_client(client).await;
    
        let store_ = store.clone();
        let ct_ = ct.clone();
        set.spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(config.ping_interval)) => {
                        let now = Utc::now();
                        let packet = ControlPacketV2::Ping(0, now, None);
    
                        if let Err(e) = store_.send_to_server(packet).await {
                            warn!("cid={} failed to send ping to tx buffer: {:?}", &client_id, e);
                            return Err(e);
                        }
                    },
                    _ = ct_.cancelled() => {
                        return Ok(());
                    }
                }
            }
        });

        tokio::select! {
            // some tasks crashed
            v = set.join_next() => {
                info!("some tasks exited: {:?}, reconnecting...", v);

                ct.cancel();
                set.shutdown().await;

                let t = match store.get_reconnect_token().await {
                    Some(t) => t,
                    None => {
                        record_error(Error::ReconnectTokenError);
                        break;
                    }
                };
                token = t;
                request_type = RequestType::Reconnect;
                continue;
            },
            // cancelled by API
            _ = cancellation_token.cancelled() => {
                info!("run_client cancelled by token");
                break;
            },
            // cancelled by terminal
            _ = signal::ctrl_c() => {
                break;
            }
        }
    }
    Ok(())
}


pub async fn send_client_hello<T>(websocket: &mut T, token: String, request_type: RequestType) -> Result<(), T::Error>
where
    T: Unpin + Sink<Message>,
{
    let hello = match request_type {
        RequestType::NewClient { endpoint_claims } => {
            ClientHelloV2 {
                version: CLIENT_HELLO_VERSION,
                token,
                endpoint_claims,
                client_type: ClientType::Auth,
            }
        }
        RequestType::Reconnect => {
            ClientHelloV2 {
                version: CLIENT_HELLO_VERSION,
                token,
                endpoint_claims: vec![],
                client_type: ClientType::Reconnect,
            }
        }
    };
    debug!("Sent client hello: {:?}", hello);
    let hello_data = serde_json::to_vec(&hello).unwrap_or_default();
    websocket.send(Message::binary(hello_data)).await?;

    Ok(())
}

// Wormhole
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub client_id: ClientId,
    pub host: String,
    pub endpoints: Endpoints,
}

pub async fn verify_server_hello<T>(websocket: &mut T) -> Result<ClientInfo, Error>
where
    T: Unpin + Stream<Item = Result<Message, WsError>>,
{
    let server_hello_data = websocket
        .next()
        .await
        .ok_or(Error::NoResponseFromServer)??
        .into_data();
    let server_hello = serde_json::from_slice::<ServerHelloV2>(&server_hello_data).map_err(|e| {
        error!("Couldn't parse server_hello from {:?}", e);
        Error::ServerReplyInvalid
    })?;
    debug!("Got server hello: {:?}", server_hello);

    let (client_id, host, endpoints) = match server_hello {
        ServerHelloV2::Success {
            client_id,
            endpoints,
            host,
        } => {
            info!("cid={} Server accepted our connection.", client_id);
            (client_id, host, endpoints)
        }
        ServerHelloV2::BadRequest => {
            error!("Server send an error: {:?}", Error::BadRequest);
            return Err(Error::BadRequest);
        }
        ServerHelloV2::ServiceTemporaryUnavailable => {
            error!(
                "Server send an error: {:?}",
                Error::ServiceTemporaryUnavailable
            );
            return Err(Error::ServiceTemporaryUnavailable);
        }
        ServerHelloV2::IllegalHost => {
            error!("Server send an error: {:?}", Error::IllegalHost);
            return Err(Error::IllegalHost);
        }
        ServerHelloV2::VersionMismatch => {
            error!(
                "Server send an error: {:?}",
                Error::ClientHandshakeVersionMismatch
            );
            return Err(Error::ClientHandshakeVersionMismatch);
        }
        ServerHelloV2::InternalServerError => {
            error!("Server send an error: {:?}", Error::InternalServerError);
            return Err(Error::InternalServerError);
        }
    };

    Ok(ClientInfo {
        client_id,
        host,
        endpoints,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum TokenResponse {
    Ok { token: String, host: String },
    Err { message: String },
}

pub async fn fetch_token(url: &str) -> Result<(String, String)> {
    let client = reqwest::Client::new();
    let resp = client
        .post(url)
        .send()
        .await?
        .json::<TokenResponse>()
        .await?;

    match resp {
        TokenResponse::Ok { token, host } => Ok((token, host)),
        TokenResponse::Err { message } => Err(anyhow!(message)),
    }
}

#[cfg(test)]
mod fetch_token_test {
    use super::fetch_token;
    use warp::{http::StatusCode, Filter};

    #[tokio::test]
    async fn parse_ok_response() -> Result<(), Box<dyn std::error::Error>> {
        let response = r#"
        {
            "token": "json.web.token",
            "host": "foo.local" 
        }"#;
        let routes = warp::any().map(move || response);
        tokio::spawn(async move {
            warp::serve(routes).run(([127, 0, 0, 1], 11111)).await;
        });

        let (token, host) = fetch_token("http://localhost:11111/v0/request_token").await?;
        assert_eq!(token, "json.web.token".to_string());
        assert_eq!(host, "foo.local".to_string());
        Ok(())
    }

    #[tokio::test]
    async fn returns_error_when_token_server_internal_error(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let response = r#"
        {
            "message": "failed to generate token"
        }"#;
        let routes = warp::any()
            .map(move || warp::reply::with_status(response, StatusCode::INTERNAL_SERVER_ERROR));
        tokio::spawn(async move {
            warp::serve(routes).run(([127, 0, 0, 1], 11112)).await;
        });

        let result = fetch_token("http://localhost:11112/v0/request_token").await;
        assert!(result.is_err());

        let error = result.err().unwrap();
        assert_eq!(error.to_string(), "failed to generate token");
        Ok(())
    }
}


#[cfg(test)]
mod client_verify_server_hello_test {
    use super::*;
    use futures::{channel::mpsc, SinkExt};
    use ownserver_lib::{ClientId, Endpoint, EndpointId, Protocol, ServerHelloV2};

    #[tokio::test]
    async fn it_accept_server_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        let cid = ClientId::new();
        let eid = EndpointId::new();
        let hello = serde_json::to_vec(&ServerHelloV2::Success {
            client_id: cid,
            host: "foo.bar.local".to_string(),
            endpoints: vec![Endpoint {
                id: eid,
                protocol: Protocol::TCP,
                local_port: 1234,
                remote_port: 1234,
            }],
        })
        .unwrap_or_default();
        tx.send(Ok(Message::binary(hello))).await?;

        let client_info = verify_server_hello(&mut rx)
            .await
            .expect("unexpected server hello error");
        let ClientInfo {
            client_id,
            host,
            endpoints,
        } = client_info;
        assert_eq!(client_id, cid);
        assert_eq!(host, "foo.bar.local".to_string());
        assert_eq!(endpoints, vec![Endpoint {
            id: eid,
            protocol: Protocol::TCP,
            local_port: 1234,
            remote_port: 1234,
        }]);

        Ok(())
    }

    #[tokio::test]
    async fn returns_errors_when_websocket_yields_nothing() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut tx, mut rx) = mpsc::unbounded();

        tx.disconnect();

        let server_hello = verify_server_hello(&mut rx)
            .await
            .expect_err("server hello is unexpectedly correct");
        assert!(matches!(server_hello, Error::NoResponseFromServer));

        Ok(())
    }

    #[tokio::test]
    async fn returns_errors_when_server_hello_is_invalid() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut tx, mut rx) = mpsc::unbounded();

        let hello = serde_json::to_vec(&"hello server").unwrap_or_default();
        tx.send(Ok(Message::binary(hello))).await?;

        let server_hello = verify_server_hello(&mut rx)
            .await
            .expect_err("server hello is unexpectedly correct");
        assert!(matches!(server_hello, Error::ServerReplyInvalid));

        Ok(())
    }

    #[tokio::test]
    async fn returns_errors_when_websocket_error() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        tx.send(Err(WsError::AlreadyClosed)).await?;

        let server_hello = verify_server_hello(&mut rx)
            .await
            .expect_err("server hello is unexpectedly correct");
        assert!(matches!(server_hello, Error::WebSocketError(_)));

        Ok(())
    }
}

#[cfg(test)]
mod calculate_reconnect_backoff_test {
    use super::*;

    #[test]
    fn it_calculates_reconnect_backoff() {
        assert_eq!(calculate_reconnect_backoff(0), Duration::from_secs(0));
        assert_eq!(calculate_reconnect_backoff(1), Duration::from_secs(1));
        assert_eq!(calculate_reconnect_backoff(2), Duration::from_secs(2));
        assert_eq!(calculate_reconnect_backoff(3), Duration::from_secs(4));
        assert_eq!(calculate_reconnect_backoff(4), Duration::from_secs(8));
        assert_eq!(calculate_reconnect_backoff(5), Duration::from_secs(16));
        assert_eq!(calculate_reconnect_backoff(6), Duration::from_secs(32));
        assert_eq!(calculate_reconnect_backoff(7), Duration::from_secs(64));
        assert_eq!(calculate_reconnect_backoff(8), Duration::from_secs(128));
        assert_eq!(calculate_reconnect_backoff(9), Duration::from_secs(256));
        assert_eq!(calculate_reconnect_backoff(10), Duration::from_secs(300));
        assert_eq!(calculate_reconnect_backoff(11), Duration::from_secs(300));
    }
}
