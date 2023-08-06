use anyhow::{anyhow, Result};
use bytes::BytesMut;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::{Sink, SinkExt, Stream, StreamExt};
use log::*;
use serde::{Deserialize, Serialize};
use tokio_util::codec::{Encoder, Decoder};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as WsError, Message},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::Error;
use crate::{local, Store};
use crate::localudp;
use crate::{StreamMessage};
use ownserver_lib::{
    ClientHello, ClientId, ControlPacket, Payload, ServerHello, CLIENT_HELLO_VERSION, ControlPacketCodec,
};

pub async fn run(
    store: Arc<Store>,
    control_port: u16,
    local_port: u16,
    token_server: &str,
    payload: Payload,
    cancellation_token: CancellationToken,
) -> Result<(ClientInfo, JoinSet<Result<(), Error>>)> {
    println!("Connecting to auth server: {}", token_server);
    let (token, host) = fetch_token(token_server).await?;
    info!("got token: {}, host: {}", token, host);
    println!("Your proxy server: {}", host);

    println!("Connecting to proxy server: {}:{}", host, control_port);
    let url = Url::parse(&format!("ws://{}:{}/tunnel", host, control_port))?;
    let (mut websocket, _) = connect_async(url).await.map_err(|_| Error::ServerDown)?;
    info!("WebSocket handshake has been successfully completed");

    send_client_hello(&mut websocket, token, payload).await?;
    let client_info = verify_server_hello(&mut websocket).await?;
    info!(
        "cid={} got client_info from server: {:?}",
        client_info.client_id, client_info
    );
    println!("Your Client ID: {}", client_info.client_id);

    // split reading and writing
    let (mut ws_sink, mut ws_stream) = websocket.split();

    // tunnel channel
    let (tunnel_tx, mut tunnel_rx) = unbounded::<ControlPacket>();

    let mut set = JoinSet::new();
    let client_id = client_info.client_id;
    let ct = cancellation_token.child_token();
    // continuously write to websocket tunnel
    set.spawn(async move {
        loop {
            tokio::select! {
                v = tunnel_rx.next() => {
                    let packet = match v {
                        Some(data) => data,
                        None => {
                            warn!("cid={} control flow didn't send anything!", client_id);
                            return Ok(());
                        }
                    };

                    let mut codec = ControlPacketCodec::new();
                    let mut bytes = BytesMut::new();
                    if let Err(e) = codec.encode(packet, &mut bytes) {
                        warn!("cid={} failed to encode message: {:?}", client_id, e);
                        return Ok(());
                    }
                    if let Err(e) = ws_sink.send(Message::binary(bytes.to_vec())).await {
                        warn!("cid={} failed to write message to tunnel websocket: {:?}", client_id, e);
                        return Ok(());
                    }

                },
                _ = ct.cancelled() => {
                    return Ok(());
                }
            }
        }
    });

    let ct = cancellation_token.child_token();
    set.spawn(async move {
        // continuously read from websocket tunnel
        loop {
            tokio::select! {
                v = ws_stream.next() => {
                    match v {
                        Some(Ok(message)) if message.is_close() => {
                            debug!("cid={} got close message", client_id);
                            return Ok(());
                        }
                        Some(Ok(message)) => {
                            let packet = process_control_flow_message(
                                store.clone(),
                                tunnel_tx.clone(),
                                message.into_data(),
                                local_port,
                            )
                            .await
                            .map_err(|e| {
                                error!("cid={} Malformed protocol control packet: {:?}", client_id, e);
                                Error::MalformedMessageFromServer
                            })?;
                            debug!("cid={} Processed data packet: {}", client_id, packet);
                        }
                        Some(Err(e)) => {
                            warn!("cid={} websocket read error: {:?}", client_id, e);
                            return Err(Error::Timeout);
                        }
                        None => {
                            warn!("cid={} websocket sent none", client_id);
                            return Err(Error::Timeout);
                        }
                    }
                },
                _ = ct.cancelled() => {
                    return Ok(());
                }
            }
        }
    });

    let message = format!("Your server {}://localhost:{} is now available at {}://{}", payload, local_port, payload, client_info.remote_addr);
    println!("+{}+", "-".repeat(message.len() + 2));
    println!("| {} |", message);
    println!("+{}+", "-".repeat(message.len() + 2));

    Ok((client_info, set))
}

pub async fn send_client_hello<T>(websocket: &mut T, token: String, payload: Payload) -> Result<(), T::Error>
where
    T: Unpin + Sink<Message>,
{
    let hello = ClientHello {
        version: CLIENT_HELLO_VERSION,
        token,
        payload,
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
    pub remote_addr: String,
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
    let server_hello = serde_json::from_slice::<ServerHello>(&server_hello_data).map_err(|e| {
        error!("Couldn't parse server_hello from {:?}", e);
        Error::ServerReplyInvalid
    })?;
    debug!("Got server hello: {:?}", server_hello);

    let (client_id, remote_addr) = match server_hello {
        ServerHello::Success {
            client_id,
            remote_addr,
            ..
        } => {
            info!("cid={} Server accepted our connection.", client_id);
            (client_id, remote_addr)
        }
        ServerHello::BadRequest => {
            error!("Server send an error: {:?}", Error::BadRequest);
            return Err(Error::BadRequest);
        }
        ServerHello::ServiceTemporaryUnavailable => {
            error!(
                "Server send an error: {:?}",
                Error::ServiceTemporaryUnavailable
            );
            return Err(Error::ServiceTemporaryUnavailable);
        }
        ServerHello::IllegalHost => {
            error!("Server send an error: {:?}", Error::IllegalHost);
            return Err(Error::IllegalHost);
        }
        ServerHello::VersionMismatch => {
            error!(
                "Server send an error: {:?}",
                Error::ClientHandshakeVersionMismatch
            );
            return Err(Error::ClientHandshakeVersionMismatch);
        }
        ServerHello::InternalServerError => {
            error!("Server send an error: {:?}", Error::InternalServerError);
            return Err(Error::InternalServerError);
        }
    };

    Ok(ClientInfo {
        client_id,
        remote_addr,
    })
}

pub async fn process_control_flow_message(
    store: Arc<Store>,
    mut tunnel_tx: UnboundedSender<ControlPacket>,
    payload: Vec<u8>,
    local_port: u16,
) -> Result<ControlPacket, Box<dyn std::error::Error>> {
    let mut bytes = BytesMut::from(&payload[..]);
    let control_packet = ControlPacketCodec::new().decode(&mut bytes)?
        .ok_or("failed to parse partial packet")?;
        // TODO: should handle None case

    match control_packet {
        ControlPacket::Init(stream_id) => {
            debug!("sid={} init stream", stream_id);

            if !store.has_stream(&stream_id) {
                local::setup_new_stream(
                    store.clone(),
                    local_port,
                    tunnel_tx.clone(),
                    stream_id,
                )
                .await;
                println!("new tcp stream arrived: {}", stream_id);
            } else {
                warn!(
                    "sid={} already exist at init process",
                    stream_id
                );
            }
        }
        ControlPacket::Ping => {
            debug!("got ping");
            let _ = tunnel_tx.send(ControlPacket::Ping).await;
        }
        ControlPacket::Refused(_) => return Err("unexpected control packet".into()),
        ControlPacket::End(stream_id) => {
            debug!("sid={} end stream", stream_id);
            // proxy server try to close control stream and local stream

            tokio::spawn(async move {
                if let Some((stream_id, mut tx)) = store.remove_stream(&stream_id) {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = tx.send(StreamMessage::Close).await.map_err(|e| {
                        error!(
                            "sid={} failed to send stream close: {:?}",
                            stream_id,
                            e
                        );
                    });
                    println!("close tcp stream: {}", stream_id);
                }
            });
        }
        ControlPacket::Data(stream_id, ref data) => {
            debug!("sid={} new data: {}", stream_id, data.len());

            match store.get_mut_stream(&stream_id) {
                Some(mut tx) => {
                    tx.send(StreamMessage::Data(data.clone())).await?;
                    debug!("sid={} forwarded to local tcp", stream_id);
                }
                None => {
                    error!(
                        "sid={} got data but no stream to send it to.",
                        stream_id
                    );
                    let _ = tunnel_tx
                        .send(ControlPacket::Refused(stream_id))
                        .await?;
                }
            }
        }
        ControlPacket::UdpData(stream_id, ref data) => {
            debug!("sid={} new data: {}", stream_id, data.len());
            // find the right stream
            if !store.has_stream(&stream_id) {
                localudp::setup_new_stream(
                    store.clone(),
                    local_port,
                    tunnel_tx.clone(),
                    stream_id,
                )
                .await;
                println!("new udp stream arrived: {}", stream_id);
            }

            // forward data to it
            if let Some(mut tx) = store.get_mut_stream(&stream_id) {
                tx.send(StreamMessage::Data(data.clone())).await?;
                debug!("sid={} forwarded to local tcp", stream_id);
            } else {
                warn!("active_stream is not yet registered {}", stream_id);
            }
        }
    };

    Ok(control_packet)
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
    use ownserver_lib::{ClientId, ServerHello};
    use tokio_tungstenite::{
        tungstenite::{Error as WsError, Message},
    };

    #[tokio::test]
    async fn it_accept_server_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        let cid = ClientId::new();
        let hello = serde_json::to_vec(&ServerHello::Success {
            client_id: cid,
            remote_addr: "foo.bar.local:256".to_string(),
        })
        .unwrap_or_default();
        tx.send(Ok(Message::binary(hello))).await?;

        let client_info = verify_server_hello(&mut rx)
            .await
            .expect("unexpected server hello error");
        let ClientInfo {
            client_id,
            remote_addr,
        } = client_info;
        assert_eq!(cid, client_id);
        assert_eq!("foo.bar.local:256".to_string(), remote_addr);

        Ok(())
    }

    #[tokio::test]
    async fn returns_errors_when_websocket_yields_nothing() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut tx, mut rx) = mpsc::unbounded();

        tx.disconnect();

        let server_hello = verify_server_hello(&mut rx)
            .await
            .err()
            .expect("server hello is unexpectedly correct");
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
            .err()
            .expect("server hello is unexpectedly correct");
        assert!(matches!(server_hello, Error::ServerReplyInvalid));

        Ok(())
    }

    #[tokio::test]
    async fn returns_errors_when_websocket_error() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        tx.send(Err(WsError::AlreadyClosed)).await?;

        let server_hello = verify_server_hello(&mut rx)
            .await
            .err()
            .expect("server hello is unexpectedly correct");
        assert!(matches!(server_hello, Error::WebSocketError(_)));

        Ok(())
    }
}