use std::sync::Arc;

use bytes::BytesMut;
use futures::{stream::SplitSink, StreamExt, SinkExt};
use metrics::gauge;
use ownserver_lib::{ClientId, Endpoints, ControlPacketV2Codec, ControlPacketV2};
use tokio_util::{sync::CancellationToken, codec::{Encoder, Decoder}};
use tracing::Instrument;
use warp::ws::{Message, WebSocket};

use crate::{Store, remote::stream::StreamMessage, ClientStreamError};
use chrono::{DateTime, Duration, Utc};


#[derive(Debug)]
pub struct Client {
    pub client_id: ClientId,
    pub endpoints: Endpoints,

    ws_tx: SplitSink<WebSocket, Message>,
    ct_self: CancellationToken,
    ct_child: CancellationToken,
    state: ClientState,
    reconnect_window: Duration,
}

#[derive(Debug)]
pub enum ClientState {
    /// client websocket is active
    Connected,

    /// client websocket was disconnected, waiting for reconnect
    WaitReconnect {
        /// if client does't reconnect within window, drop streams associated with this client
        expires: DateTime<Utc>
    },
}

impl Client {
    pub fn new(store: Arc<Store>, client_id: ClientId, endpoints: Endpoints, ws: WebSocket, reconnect_window: Duration) -> Self {
        let (sink, mut stream) = ws.split();
        let token = CancellationToken::new();

        let ct: CancellationToken = token.clone();
        let store_ = store.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
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
                
                        let mut bytes = BytesMut::from(&message[..]);
                        let packet = match ControlPacketV2Codec::new().decode(&mut bytes) {
                            Ok(Some(packet)) => packet,
                            Ok(None) => {
                                // TODO: should handle None case
                                tracing::warn!(cid = %client_id, "failed to parse partial client message");
                                continue;
                            }
                            Err(e) => {
                                tracing::warn!(cid = %client_id, error = ?e, "failed to parse client message");
                                continue;
                            }
                        };


                
                        tracing::debug!(cid = %client_id, ?packet, "got control packet from client");

                        let (stream_id, message) = match packet {
                            ControlPacketV2::Data(stream_id, data) => {
                                tracing::trace!(cid = %client_id, sid = %stream_id, "forwarding to stream: {}", data.len());
                                (stream_id, StreamMessage::Data(data))
                            }
                            ControlPacketV2::Refused(stream_id) => {
                                tracing::debug!(cid = %client_id, sid = %stream_id, "tunnel says: refused");
                                (stream_id, StreamMessage::TunnelRefused)
                            }
                            ControlPacketV2::Ping(seq, datetime, None) => {
                                tracing::trace!(cid = %client_id, seq = %seq, datetime=%datetime, "pong");
                                let _ = store_.send_to_client(client_id, ControlPacketV2::Pong(seq, datetime)).await;
                                continue;
                            }
                            ControlPacketV2::Pong(_, datetime) => {
                                tracing::trace!(cid = %client_id, "pong");

                                let current_time = Utc::now();
                                let rtt = current_time.signed_duration_since(datetime).num_milliseconds() as f64;
                                gauge!("ownserver_server.stream.rtt", rtt, "client_id" => client_id.to_string());
                                continue;
                            }
                            ControlPacketV2::Ping(seq, datetime, Some(token)) => {
                                tracing::error!(cid = %client_id, seq = %seq, datetime=%datetime, token=%token, "invalid protocol ControlPacketV2::Ping, client cannot send token");
                                let _ = store_.send_to_client(client_id, ControlPacketV2::Pong(seq, datetime)).await;
                                continue;
                            }
                            ControlPacketV2::Init(stream_id, endpoint_id, remote_info) => {
                                tracing::error!(cid = %client_id, sid = %stream_id, eid = %endpoint_id, remote_info = %remote_info, "invalid protocol ControlPacketV2::Init");
                                continue;
                            }
                            ControlPacketV2::End(stream_id) => {
                                tracing::error!(cid = %client_id, sid = %stream_id, "invalid protocol ControlPacketV2::End");
                                continue;
                            }
                        };

                        tracing::trace!(cid = %client_id, sid = %stream_id, "forward message to remote stream");
                        if let Err(e) = store_.send_to_remote(stream_id, message).await {
                            tracing::debug!(cid = %client_id, sid = %stream_id, error = ?e, "Failed to send to remote stream");

                            store_.disable_remote(stream_id).await;
                        }
                    }
                }
            }
            store_.set_wait_reconnect(client_id).await;
        }.instrument(tracing::info_span!("client_read_loop")));

        Self {
            client_id,
            endpoints,
            ws_tx: sink,
            ct_self: token,
            ct_child: CancellationToken::new(),
            state: ClientState::Connected,
            reconnect_window,
        }
    }

    pub async fn send_to_client(&mut self, packet: ControlPacketV2) -> Result<(), ClientStreamError> {
        let mut codec = ControlPacketV2Codec::new();
        let mut bytes = BytesMut::new();
        if let Err(e) = codec.encode(packet, &mut bytes) {
            tracing::warn!(cid = %self.client_id, error = ?e, "failed to encode message");
            return Err(ClientStreamError::ClientError(format!("packet is invalid {}", e)))
        }

        if let Err(e) =  self.ws_tx.send(Message::binary(bytes.to_vec())).await {
            tracing::debug!(cid = %self.client_id, error = ?e, "client disconnected: aborting");
            self.set_wait_reconnect();
            return Err(ClientStreamError::ClientError(format!("failed to communicate with client {:?}", e)))
        }
        Ok(())
    }

    pub fn set_wait_reconnect(&mut self) {
        if let ClientState::Connected = self.state {
            self.state = ClientState::WaitReconnect { expires: Utc::now() + self.reconnect_window };
            tracing::debug!(cid = %self.client_id, "set client state: {:?}", self.state);
        }
    }

    pub fn can_cleanup(&self) -> bool {
        matches!(self.state, ClientState::WaitReconnect { expires } if Utc::now() >= expires)
    }

    pub fn clone_child_token(&self) -> CancellationToken {
        self.ct_child.clone()
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

}

impl Drop for Client {
    fn drop(&mut self) {
        self.ct_self.cancel();

        match self.state {
            ClientState::Connected => {
                tracing::warn!(cid = %self.client_id, "dropping connected client");
            },
            ClientState::WaitReconnect { expires } if Utc::now() < expires => {
                tracing::info!(cid = %self.client_id, "dropping client, retain streams");
            },
            ClientState::WaitReconnect { .. } => {
                tracing::info!(cid = %self.client_id, "droppping client and associated streams");
                self.ct_child.cancel();
            }
        }
    }
}