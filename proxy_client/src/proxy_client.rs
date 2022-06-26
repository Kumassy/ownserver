use anyhow::{anyhow, Result};
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::{Sink, SinkExt, Stream, StreamExt};
use log::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as WsError, Message},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::Error;
use crate::local;
use crate::localudp;
use crate::{ActiveStreams, StreamMessage};
use magic_tunnel_lib::{
    ClientHello, ClientId, ControlPacket, Payload, ServerHello, StreamId, CLIENT_HELLO_VERSION,
};

pub async fn run(
    active_streams: &'static ActiveStreams,
    control_port: u16,
    local_port: u16,
    token_server: &str,
    payload: Payload,
    cancellation_token: CancellationToken,
) -> Result<(ClientInfo, JoinHandle<Result<(), Error>>)> {
    let (token, host) = fetch_token(token_server).await?;
    info!("got token: {}, host: {}", token, host);

    let url = Url::parse(&format!("wss://{}:{}/tunnel", host, control_port))?;
    let (mut websocket, _) = connect_async(url).await.map_err(|_| Error::ServerDown)?;
    info!("WebSocket handshake has been successfully completed");

    send_client_hello(&mut websocket, token, payload).await?;
    let client_info = verify_server_hello(&mut websocket).await?;
    info!(
        "cid={} got client_info from server: {:?}",
        client_info.client_id, client_info
    );

    // split reading and writing
    let (mut ws_sink, mut ws_stream) = websocket.split();

    // tunnel channel
    let (tunnel_tx, mut tunnel_rx) = unbounded::<ControlPacket>();

    let client_id = client_info.client_id.clone();
    let client_id_clone = client_id.clone();
    let ct = cancellation_token.child_token();
    // continuously write to websocket tunnel
    let handle_client_to_control: JoinHandle<()> = tokio::spawn(async move {
        let client_id = client_id_clone;
        loop {
            tokio::select! {
                v = tunnel_rx.next() => {
                    let packet = match v {
                        Some(data) => data,
                        None => {
                            warn!("cid={} control flow didn't send anything!", client_id);
                            return;
                        }
                    };
                    if let Err(e) = ws_sink.send(Message::binary(packet.serialize())).await {
                        warn!("cid={} failed to write message to tunnel websocket: {:?}", client_id, e);
                        return;
                    }
                },
                _ = ct.cancelled() => {
                    return;
                }
            }
        }
    });

    let ct = cancellation_token.child_token();
    let handle_control_to_client: JoinHandle<Result<(), Error>> = tokio::spawn(async move {
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
                                active_streams.clone(),
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

    let handle = tokio::spawn(async move {
        match tokio::join!(handle_client_to_control, handle_control_to_client) {
            (Ok(_), Ok(Ok(_))) => Ok(()),
            (Ok(_), Ok(Err(e))) => Err(e),
            (Err(join_error), _) => Err(join_error.into()),
            (_, Err(join_error)) => Err(join_error.into()),
        }
    });

    Ok((client_info, handle))
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

// TODO: improve testability, fix return value
pub async fn process_control_flow_message(
    active_streams: ActiveStreams,
    mut tunnel_tx: UnboundedSender<ControlPacket>,
    payload: Vec<u8>,
    local_port: u16,
) -> Result<ControlPacket, Box<dyn std::error::Error>> {
    let control_packet = ControlPacket::deserialize(&payload)?;

    match &control_packet {
        ControlPacket::Init(stream_id) => {
            debug!("sid={} init stream", stream_id.to_string());

            if !active_streams.read().unwrap().contains_key(&stream_id) {
                local::setup_new_stream(
                    active_streams.clone(),
                    local_port,
                    tunnel_tx.clone(),
                    stream_id.clone(),
                )
                .await;
            } else {
                warn!(
                    "sid={} already exist at init process",
                    stream_id.to_string()
                );
            }
        }
        ControlPacket::Ping => {
            debug!("got ping");
            let _ = tunnel_tx.send(ControlPacket::Ping).await;
        }
        ControlPacket::Refused(_) => return Err("unexpected control packet".into()),
        ControlPacket::End(stream_id) => {
            debug!("sid={} end stream", stream_id.to_string());
            // proxy server try to close control stream and local stream

            // find the stream
            let stream_id = stream_id.clone();

            tokio::spawn(async move {
                let stream = active_streams.read().unwrap().get(&stream_id).cloned();
                if let Some(mut tx) = stream {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = tx.send(StreamMessage::Close).await.map_err(|e| {
                        error!(
                            "sid={} failed to send stream close: {:?}",
                            stream_id.to_string(),
                            e
                        );
                    });
                    active_streams.write().unwrap().remove(&stream_id);
                }
            });
        }
        ControlPacket::Data(stream_id, data) => {
            debug!("sid={} new data: {}", stream_id.to_string(), data.len());
            // find the right stream
            let active_stream = active_streams.read().unwrap().get(&stream_id).cloned();

            // forward data to it
            if let Some(mut tx) = active_stream {
                tx.send(StreamMessage::Data(data.clone())).await?;
                debug!("sid={} forwarded to local tcp", stream_id.to_string());
            } else {
                error!(
                    "sid={} got data but no stream to send it to.",
                    stream_id.to_string()
                );
                let _ = tunnel_tx
                    .send(ControlPacket::Refused(stream_id.clone()))
                    .await?;
            }
        }
        ControlPacket::UdpData(stream_id, data) => {
            debug!("sid={} new data: {}", stream_id.to_string(), data.len());
            // find the right stream
            let active_stream = active_streams.read().unwrap().get(&stream_id).cloned();

            // forward data to it
            if active_stream.is_none() {
                localudp::setup_new_stream(
                    active_streams.clone(),
                    local_port,
                    tunnel_tx.clone(),
                    stream_id.clone(),
                )
                .await;
            }

            let active_stream = active_streams.read().unwrap().get(&stream_id).cloned();
            if let Some(mut tx) = active_stream {
                tx.send(StreamMessage::Data(data.clone())).await?;
                debug!("sid={} forwarded to local tcp", stream_id.to_string());
            } else {
                warn!("active_stream is not yet registered {}", stream_id);
            }
        }
    };

    Ok(control_packet.clone())
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
