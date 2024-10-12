use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;


use bytes::BytesMut;
use chrono::Utc;
use dashmap::DashMap;
use dashmap::mapref::one::{Ref, RefMut};
use error::Error;
use futures::channel::mpsc::{SendError, UnboundedSender};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use ownserver_lib::{ClientId, ControlPacketV2, ControlPacketV2Codec, Endpoint, EndpointId, Endpoints, Protocol, RemoteInfo, StreamId};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tokio_util::codec::{Encoder, Decoder};
use log::*;

#[derive(Debug, Clone)]
pub enum StreamMessage {
    Data(Vec<u8>),
    Close,
}
pub mod error;
pub mod local;
pub mod proxy_client;
pub mod api;

#[derive(Debug, Clone)]
pub struct LocalStream {
    stream: UnboundedSender<StreamMessage>,
    remote_info: RemoteInfo,
}

impl LocalStream {
    pub fn new(stream: UnboundedSender<StreamMessage>, remote_info: RemoteInfo) -> Self {
        Self {
            stream,
            remote_info,
        }
    }

    pub async fn send_to_local(&mut self, message: StreamMessage) -> Result<(), SendError> {
        self.stream.send(message).await
    }

    pub fn remote_info(&self) -> &RemoteInfo {
        &self.remote_info
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalStreamEntry {
    pub stream_id: StreamId,
    pub remote_info: RemoteInfo,
}

#[derive(Debug)]
pub struct Client {
    pub client_id: ClientId,
    ws_tx: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    store: Arc<Store>,
    ct: CancellationToken,
}

impl Client {
    pub fn new(set: &mut JoinSet<Result<(), Error>>, store: Arc<Store>, client_id: ClientId, websocket: WebSocketStream<MaybeTlsStream<TcpStream>>, token: CancellationToken) -> Self {
        let (ws_tx, mut ws_stream) = websocket.split();
        
        let ct = token.clone();
        let store_ = store.clone();
        set.spawn(async move {
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
                                    store_.clone(),
                                    message.into_data(),
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
        
        Self {
            store,
            client_id,
            ws_tx,
            ct: token,
        }
    }

    pub async fn send_to_server(&mut self, packet: ControlPacketV2) -> Result<(), Error> {
        let mut codec = ControlPacketV2Codec::new();
        let mut bytes = BytesMut::new();
        if let Err(e) = codec.encode(packet, &mut bytes) {
            warn!("cid={} failed to encode message: {:?}", self.client_id, e);
            
            // TODO: Implement Error
            return Ok(());
        }
        if let Err(e) = self.ws_tx.send(Message::binary(bytes.to_vec())).await {
            warn!("cid={} failed to write message to tunnel websocket: {:?}", self.client_id, e);

            return Err(Error::WebSocketError(e));
        }

        Ok(())
    }
}

async fn process_control_flow_message(
    store: Arc<Store>,
    payload: Vec<u8>,
) -> Result<ControlPacketV2, Box<dyn std::error::Error>> {
    let mut bytes = BytesMut::from(&payload[..]);
    let control_packet = ControlPacketV2Codec::new().decode(&mut bytes)?
        .ok_or("failed to parse partial packet")?;
        // TODO: should handle None case

    match control_packet {
        // TODO: use remote_info
        ControlPacketV2::Init(stream_id, endpoint_id, ref remote_info) => {
            debug!("sid={} eid={} remote_info={} init stream", stream_id, endpoint_id, remote_info);

            let endpoint = match store.get_endpoint_by_endpoint_id(endpoint_id) {
                Some(e) => e,
                None => {
                    warn!(
                        "sid={} eid={} endpoint is not registered",
                        stream_id, endpoint_id
                    );
                    return Err(format!("eid={} is not registered", endpoint_id).into())
                }
            };

            if store.has_stream(&stream_id) {
                warn!(
                    "sid={} already exist at init process",
                    stream_id
                );
                return Err(format!("sid={} is already exist", stream_id).into())
            }

            match endpoint.protocol {
                Protocol::TCP => {
                    local::tcp::setup_new_stream(
                        store.clone(),
                        stream_id,
                        endpoint_id,
                        remote_info.clone(),
                    )
                    .await?;
                    println!("new tcp stream arrived: sid={}, eid={}", stream_id, endpoint_id);
                }
                Protocol::UDP => {
                    local::udp::setup_new_stream(
                        store.clone(),
                        stream_id,
                        endpoint_id,
                        remote_info.clone(),
                    )
                    .await?;
                    println!("new udp stream arrived: sid={}, eid={}", stream_id, endpoint_id);
                }
            }
        }
        ControlPacketV2::Ping(seq, datetime) => {
            debug!("got ping");
            let _ = store.send_to_server(ControlPacketV2::Pong(seq, datetime)).await;
        }
        ControlPacketV2::Pong(_, datetime) => {
            debug!("got pong");
            // calculate RTT
            let current_time = Utc::now();
            let rtt = current_time.signed_duration_since(datetime).num_milliseconds();
            store.set_rtt(rtt);
            debug!("RTT: {}ms", rtt);
        }
        ControlPacketV2::Refused(_) => return Err("unexpected control packet".into()),
        ControlPacketV2::End(stream_id) => {
            debug!("sid={} end stream", stream_id);
            // proxy server try to close control stream and local stream

            tokio::spawn(async move {
                if let Some((stream_id, mut tx)) = store.remove_stream(&stream_id) {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = tx.send_to_local(StreamMessage::Close).await.map_err(|e| {
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
        ControlPacketV2::Data(stream_id, ref data) => {
            debug!("sid={} new data: {}", stream_id, data.len());

            match store.get_mut_stream(&stream_id) {
                Some(mut tx) => {
                    tx.send_to_local(StreamMessage::Data(data.clone())).await?;
                    debug!("sid={} forwarded to local socket", stream_id);
                }
                None => {
                    error!(
                        "sid={} got data but no stream to send it to.",
                        stream_id
                    );
                    store.send_to_server(ControlPacketV2::Refused(stream_id))
                        .await?;
                }
            }
        }
    };

    Ok(control_packet)
}

#[derive(Debug, Default)]
pub struct Store {
    streams: DashMap<StreamId, LocalStream>,
    client: Mutex<Option<Client>>,
    endpoints_map: DashMap<EndpointId, Endpoint>,
    rtt: AtomicI64,
}

impl Store {
    pub fn add_stream(&self, stream_id: StreamId, stream: LocalStream) {
        self.streams.insert(stream_id, stream);
    }

    pub fn remove_stream(&self, stream_id: &StreamId) -> Option<(StreamId, LocalStream)> {
        self.streams.remove(stream_id)
    }

    pub fn has_stream(&self, stream_id: &StreamId) -> bool {
        self.streams.contains_key(stream_id)
    }

    pub fn get_stream(&self, stream_id: &StreamId) -> Option<Ref<StreamId, LocalStream>> {
        self.streams.get(stream_id)
    }

    pub fn get_mut_stream(&self, stream_id: &StreamId) -> Option<RefMut<StreamId, LocalStream>> {
        self.streams.get_mut(stream_id)
    }

    pub fn len_stream(&self) -> usize {
        self.streams.len()
    }

    pub fn list_streams(&self) -> Vec<LocalStreamEntry> {
        self.streams.iter().map(|x|
            LocalStreamEntry { 
                stream_id: *x.key(),
                remote_info: x.value().remote_info().clone(),
             }
        ).collect()
    }

    pub fn register_endpoints(&self, endpoints: Vec<Endpoint>) {
        for endpoint in endpoints {
            self.endpoints_map.insert(endpoint.id, endpoint);
        }
    }

    pub fn get_local_addr_by_endpoint_id(&self, eid: EndpointId) -> Option<impl ToSocketAddrs + std::fmt::Debug + Clone> {
        let endpoint = self.endpoints_map.get(&eid)?;

        Some(format!("localhost:{}", endpoint.local_port))
    }

    pub fn get_endpoint_by_endpoint_id(&self, eid: EndpointId) -> Option<Endpoint> {
        self.endpoints_map.get(&eid).map(|e| e.value().clone())
    }

    pub fn get_endpoints(&self) -> Endpoints {
        self.endpoints_map.iter().map(|e| e.value().clone()).collect()
    }

    pub fn set_rtt(&self, rtt: i64) {
        self.rtt.store(rtt, Ordering::Relaxed);
    }

    pub fn get_rtt(&self) -> i64 {
        self.rtt.load(Ordering::Relaxed)
    }

    pub async fn add_client(&self, client: Client) {
        let mut c = self.client.lock().await;
        *c = Some(client);
    }

    pub async fn remove_client(&self) {
        let mut c = self.client.lock().await;
        *c = None;
    }

    pub async fn send_to_server(&self, packet: ControlPacketV2) -> Result<(), Error>  {
        if let Some(ref mut client) = self.client.lock().await.as_mut() {
            client.send_to_server(packet).await
        } else {
            // TODO: Implement Error
            Err(Error::ServerDown)
        }
    }

}
