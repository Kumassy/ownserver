use futures::{
    channel::mpsc::{unbounded, SendError},
    Sink, SinkExt, Stream, StreamExt,
};
use log::*;
pub use magic_tunnel_lib::{ClientHello, ClientId, ControlPacket, ServerHello, StreamId};
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

use crate::active_stream::{ActiveStream, ActiveStreams, StreamMessage};
use crate::connected_clients::{ConnectedClient, Connections};
use crate::port_allocator::PortAllocator;
use crate::remote::{self, CancelHander};

pub fn spawn<A: Into<SocketAddr>>(
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
    remote_cancellers: Arc<DashMap<ClientId, CancelHander>>,
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
    pub port: u16,
}

async fn try_client_handshake(
    websocket: &mut WebSocket,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
) -> Option<ClientHandshake> {
    let client_hello = match verify_client_handshake(websocket).await {
        Some(client_hello) => client_hello,
        _ => {
            tracing::error!("failed to parse client hello");
            return None;
        }
    };

    // TODO: initialization of StdRng may takes time
    let mut rng = StdRng::from_entropy();
    let port = match alloc.lock().await.allocate_port(&mut rng) {
        Ok(port) => port,
        Err(_) => {
            tracing::error!("failed to allocate port");
            return None;
        }
    };

    let client_id = match respond_with_server_hello(websocket, port).await {
        Ok(ServerHello::Success {
            client_id,
            assigned_port,
            version,
        }) => client_id,
        Err(error) => {
            tracing::info!("failed to send server hello: {}", error);
            return None;
        }
        _ => unimplemented!(),
    };

    Some(ClientHandshake {
        id: client_id,
        port,
    })
}
// async fn verify_client_handshake(websocket: &mut WebSocket) -> Option<ClientHello> {
async fn verify_client_handshake(
    websocket: &mut (impl Unpin + Stream<Item = Result<Message, WarpError>>),
) -> Option<ClientHello> {
    let client_hello_data = match websocket.next().await {
        Some(Ok(msg)) if (msg.is_binary() || msg.is_text()) && !msg.as_bytes().is_empty() => {
            msg.into_bytes()
        }
        _ => {
            tracing::error!("no client init message");
            return None;
        }
    };

    let client_hello: ClientHello = match serde_json::from_slice(&client_hello_data) {
        Ok(client_hello) => client_hello,
        _ => {
            tracing::error!("failed to deserialize client hello");
            return None;
        }
    };
    Some(client_hello)
}

// async fn respond_with_server_hello(websocket: &mut (impl Unpin + Sink<Message>)) -> Result<(), WarpError>
async fn respond_with_server_hello<T>(websocket: &mut T, port: u16) -> Result<ServerHello, T::Error>
where
    T: Unpin + Sink<Message>,
{
    // Send server hello success
    let client_id = ClientId::generate();

    let server_hello = ServerHello::Success {
        client_id: client_id.clone(),
        assigned_port: port,
        version: 0,
    };
    let data = serde_json::to_vec(&server_hello).unwrap_or_default();

    websocket.send(Message::binary(data.clone())).await?;

    Ok(server_hello)
}

async fn handle_new_connection(
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
    remote_cancellers: Arc<DashMap<ClientId, CancelHander>>,
    client_ip: SocketAddr,
    mut websocket: WebSocket,
) {
    let handshake = match try_client_handshake(&mut websocket, alloc).await {
        Some(ws) => ws,
        None => return,
    };
    let client_id = handshake.id;
    tracing::info!(client_ip=%client_ip, id=%client_id, port=%handshake.port, "open tunnel");
    let host = format!("host-foobar-{}", handshake.port);
    let listen_addr = format!("[::]:{}", handshake.port);
    let canceller =
        match remote::spawn_remote(conn, active_streams, listen_addr, host.clone()).await {
            Ok(canceller) => canceller,
            Err(_) => {
                tracing::error!("failed to bind to allocated port");
                return;
            }
        };
    remote_cancellers.insert(client_id.clone(), canceller);

    let (tx, rx) = unbounded::<ControlPacket>();
    let client = ConnectedClient {
        id: client_id.clone(),
        host,
        tx,
    };
    Connections::add(conn, client.clone());
    let active_streams = active_streams.clone();
    let (sink, stream) = websocket.split();

    let client_clone = client.clone();
    let remote_cancellers_clone = remote_cancellers.clone();
    let client_id_clone = client_id.clone();
    tokio::spawn(
        async move {
            let client = tunnel_client(client_clone, sink, rx).await;
            Connections::remove(conn, &client);
            if let Some((cid, handler)) = remote_cancellers_clone.remove(&client_id_clone) {
                if let Err(_) = handler.send(()) {
                    tracing::error!("failed to cancel remote for {:?}", client_id_clone);
                }
            }
        }
        .instrument(tracing::info_span!("tunnel_client")),
    );
    let client_clone = client.clone();
    tokio::spawn(
        async move {
            let client = process_client_messages(active_streams, client_clone, stream).await;
            Connections::remove(conn, &client);
            if let Some((cid, handler)) = remote_cancellers.remove(&client_id) {
                if let Err(_) = handler.send(()) {
                    tracing::error!("failed to cancel remote for {:?}", client_id);
                }
            }
        }
        .instrument(tracing::info_span!("process_client")),
    );
}

/// Send the client a "stream init" message
pub async fn send_client_stream_init(mut stream: ActiveStream) -> Result<(), SendError> {
    stream
        .client
        .tx
        .send(ControlPacket::Init(stream.id.clone()))
        .await
}

/// Process client control messages
#[must_use]
#[tracing::instrument(skip(client_conn, active_streams))]
// pub async fn process_client_messages(active_streams: ActiveStreams, connections: &Connections, client: ConnectedClient, mut client_conn: SplitStream<WebSocket>) {
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
                tracing::debug!(?client.id, "goodbye client");
                return client;
            }
        };

        let packet = match ControlPacket::deserialize(&message) {
            Ok(packet) => packet,
            Err(e) => {
                error!("invalid data packet {:?}", e);
                continue;
            }
        };

        let (stream_id, message) = match packet {
            ControlPacket::Data(stream_id, data) => {
                tracing::debug!(?stream_id, num_bytes=?data.len(), "forwarding to stream");
                (stream_id, StreamMessage::Data(data))
            }
            ControlPacket::Refused(stream_id) => {
                tracing::debug!("tunnel says: refused");
                (stream_id, StreamMessage::TunnelRefused)
            }
            ControlPacket::Init(_) | ControlPacket::End(_) => {
                error!("invalid protocol control::init message");
                continue;
            }
            ControlPacket::Ping => {
                tracing::trace!("pong");
                // Connections::add(connections, client.clone());
                continue;
            }
        };

        let stream = active_streams.get(&stream_id).map(|s| s.value().clone());

        info!("found stream: {:?}", stream);
        if let Some(mut stream) = stream {
            let _ = stream.tx.send(message).await.map_err(|error| {
                tracing::trace!(?error, "Failed to send to stream tx");
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
#[tracing::instrument(skip(sink, queue))]
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
                let result = sink.send(Message::binary(packet.serialize())).await;
                if let Err(error) = result {
                    tracing::trace!(?error, "client disconnected: aborting.");
                    return client;
                }
            }
            None => {
                tracing::debug!("ending client tunnel");
                return client;
            }
        };
    }
}

#[cfg(test)]
mod verify_client_handshake_test {
    use super::*;
    use futures::channel::mpsc;

    #[tokio::test]
    async fn accept_client_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        let hello = serde_json::to_vec(&ClientHello {
            id: ClientId::generate(),
            version: 0,
        })
        .unwrap_or_default();

        tx.send(Ok(Message::binary(hello))).await?;
        let hello = verify_client_handshake(&mut rx).await;
        assert!(hello.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_text_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        tx.send(Ok(Message::text("foobarbaz".to_string()))).await?;
        let hello = verify_client_handshake(&mut rx).await;
        assert!(hello.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_serialized_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        let hello = serde_json::to_vec(&"malformed".to_string()).unwrap_or_default();

        tx.send(Ok(Message::binary(hello))).await?;
        let hello = verify_client_handshake(&mut rx).await;
        assert!(hello.is_none());
        Ok(())
    }
}

#[cfg(test)]
mod respond_with_server_hello_test {
    use super::*;
    use futures::channel::mpsc;

    #[tokio::test]
    async fn it_works() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        respond_with_server_hello(&mut tx, 12345)
            .await
            .expect("failed to write to websocket");

        let server_hello_data = rx.next().await.unwrap().into_bytes();
        let server_hello: ServerHello =
            serde_json::from_slice(&server_hello_data).expect("server hello is malformed");

        assert!(matches!(server_hello, ServerHello::Success{ .. }));
        Ok(())
    }
}

#[cfg(test)]
mod process_client_messages_test {
    use super::*;
    use dashmap::DashMap;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use std::sync::Arc;

    async fn send_messages_to_client_and_process_client_message(
        is_add_stream_to_streams: bool,
        messages: Vec<Box<dyn Fn(StreamId) -> Message>>,
    ) -> Result<UnboundedReceiver<StreamMessage>, Box<dyn std::error::Error>> {
        let (mut stream_tx, stream_rx) = unbounded::<Result<Message, WarpError>>();
        let active_streams = Arc::new(DashMap::new());

        let (tx, _rx) = unbounded::<ControlPacket>();
        let client = ConnectedClient {
            id: ClientId::generate(),
            host: "foobar".into(),
            tx,
        };

        let (active_stream, queue_rx) = ActiveStream::new(client.clone());
        let stream_id = active_stream.id.clone();
        if is_add_stream_to_streams {
            active_streams.insert(stream_id.clone(), active_stream.clone());
        }

        for message in messages {
            let msg = message(stream_id.clone());
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
            Message::binary(packet.serialize())
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
            Message::binary(packet.serialize())
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
            Message::binary(packet.serialize())
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
        let client_id = ClientId::generate();
        let client = ConnectedClient {
            id: client_id.clone(),
            host: "foobar".into(),
            tx,
        };

        let (ws_tx, ws_rx) = unbounded::<Message>();
        let (mut control_tx, control_rx) = unbounded::<ControlPacket>();
        let stream_id = StreamId::generate();

        for packet in packets {
            let pkt = packet(stream_id.clone());
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
        let packet = ControlPacket::deserialize(&payload)?;
        assert_eq!(packet, ControlPacket::Init(stream_id.clone()));
        assert_eq!(ws_rx.next().await, None);

        Ok(())
    }
}
