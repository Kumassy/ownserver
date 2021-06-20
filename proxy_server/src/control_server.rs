pub use magic_tunnel_lib::{StreamId, ClientId, ClientHello, ServerHello, ControlPacket};
use futures::{SinkExt, StreamExt, Stream, Sink, AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt, stream::{SplitStream, SplitSink}, channel::mpsc::{UnboundedReceiver, SendError}};
// use log::*;
use std::net::{SocketAddr, IpAddr};
use tokio::task::JoinHandle;
use tracing::{error, info, Instrument};
use tracing_subscriber;
use warp::{Filter, Rejection, ws::{Ws, WebSocket, Message}, Error as WarpError};
use std::convert::Infallible;

use crate::connected_clients::{ConnectedClient, Connections};
use crate::active_stream::{ActiveStreams, ActiveStream, StreamMessage};


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
#[tracing::instrument(skip(client_conn))]
// pub async fn process_client_messages(active_streams: ActiveStreams, connections: &Connections, client: ConnectedClient, mut client_conn: SplitStream<WebSocket>) {
pub async fn process_client_messages<T>(active_streams: ActiveStreams, client: ConnectedClient, mut client_conn: T) -> ConnectedClient
where T: Stream<Item=Result<Message, WarpError>> + Unpin {
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
            Err(error) => {
                error!(?error, "invalid data packet");
                continue;
            }
        };

        let (stream_id, message) = match packet {
            ControlPacket::Data(stream_id, data) => {
                tracing::debug!(?stream_id, num_bytes=?data.len(),"forwarding to stream");
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
where T: Sink<Message> + Unpin, U: Stream<Item=ControlPacket> + Unpin, T::Error: std::fmt::Debug
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
mod process_client_messages_test {
    use super::*;
    use futures::channel::mpsc::unbounded;
    use dashmap::DashMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn discard_control_packet_data_no_active_stream() -> Result<(), Box<dyn std::error::Error>> {
        let (mut stream_tx, stream_rx) = unbounded::<Result<Message, WarpError>>();

        let active_streams = Arc::new(DashMap::new());
        let (tx, _rx) = unbounded::<ControlPacket>();
        let client = ConnectedClient {
            id: ClientId::generate(),
            host: "foobar".into(),
            tx
        };

        let (active_stream, mut queue_rx) = ActiveStream::new(client.clone());
        let stream_id = active_stream.id.clone();
        let packet = ControlPacket::Data(stream_id, b"foobarbaz".to_vec());
        stream_tx.send(Ok(Message::binary(packet.serialize()))).await?;
        stream_tx.close_channel();
        let _ = process_client_messages(active_streams, client, stream_rx).await;

        // all active stream must be dropped
        drop(active_stream);
        assert_eq!(queue_rx.next().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn forward_control_packet_data_to_appropriate_stream() -> Result<(), Box<dyn std::error::Error>> {
        // tracing_subscriber::fmt::init();
        let (mut stream_tx, stream_rx) = unbounded::<Result<Message, WarpError>>();

        let active_streams = Arc::new(DashMap::new());
        let (tx, _rx) = unbounded::<ControlPacket>();
        let client_id = ClientId::generate();
        let client = ConnectedClient {
            id: client_id.clone(),
            host: "foobar".into(),
            tx
        };

        let (active_stream, mut queue_rx) = ActiveStream::new(client.clone());
        let stream_id = active_stream.id.clone();
        active_streams.insert(stream_id.clone(), active_stream.clone());

        let packet = ControlPacket::Data(stream_id, b"foobarbaz".to_vec());
        stream_tx.send(Ok(Message::binary(packet.serialize()))).await?;
        stream_tx.close_channel();
        let _ = process_client_messages(active_streams, client, stream_rx).await;


        // ControlPacket::Data must be sent to ActiveStream
        assert_eq!(queue_rx.next().await, Some(StreamMessage::Data(b"foobarbaz".to_vec())));
        drop(active_stream); // all active stream must be dropped
        assert_eq!(queue_rx.next().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn forward_control_packet_refused_to_appropriate_stream() -> Result<(), Box<dyn std::error::Error>> {
        // tracing_subscriber::fmt::init();
        let (mut stream_tx, stream_rx) = unbounded::<Result<Message, WarpError>>();

        let active_streams = Arc::new(DashMap::new());
        let (tx, _rx) = unbounded::<ControlPacket>();
        let client_id = ClientId::generate();
        let client = ConnectedClient {
            id: client_id.clone(),
            host: "foobar".into(),
            tx
        };

        let (active_stream, mut queue_rx) = ActiveStream::new(client.clone());
        let stream_id = active_stream.id.clone();
        active_streams.insert(stream_id.clone(), active_stream.clone());

        let packet = ControlPacket::Refused(stream_id);
        stream_tx.send(Ok(Message::binary(packet.serialize()))).await?;
        stream_tx.close_channel();
        let _ = process_client_messages(active_streams, client, stream_rx).await;


        // ControlPacket::Data must be sent to ActiveStream
        assert_eq!(queue_rx.next().await, Some(StreamMessage::TunnelRefused));
        drop(active_stream); // all active stream must be dropped
        assert_eq!(queue_rx.next().await, None);

        Ok(())
    }

    #[tokio::test]
    async fn close_stream_remove_client() -> Result<(), Box<dyn std::error::Error>> {
        // tracing_subscriber::fmt::init();
        let (mut stream_tx, stream_rx) = unbounded::<Result<Message, WarpError>>();

        let active_streams = Arc::new(DashMap::new());
        let (tx, _rx) = unbounded::<ControlPacket>();
        let client_id = ClientId::generate();
        let client = ConnectedClient {
            id: client_id.clone(),
            host: "foobar".into(),
            tx
        };

        let (active_stream, mut queue_rx) = ActiveStream::new(client.clone());
        let stream_id = active_stream.id.clone();
        active_streams.insert(stream_id.clone(), active_stream.clone());

        stream_tx.send(Ok(Message::close())).await?;
        stream_tx.close_channel();
        let _ = process_client_messages(active_streams, client, stream_rx).await;

        drop(active_stream); // all active stream must be dropped
        assert_eq!(queue_rx.next().await, None);

        Ok(())
    }
}

#[cfg(test)]
mod tunnel_client_test {
    use super::*;
    use futures::channel::mpsc::unbounded;

    #[tokio::test]
    async fn forward_control_packet_to_websocket() -> Result<(), Box<dyn std::error::Error>> {
        let (tx, _rx) = unbounded::<ControlPacket>();
        let client_id = ClientId::generate();
        let client = ConnectedClient {
            id: client_id.clone(),
            host: "foobar".into(),
            tx
        };

        let (ws_tx, mut ws_rx) = unbounded::<Message>();
        let (mut control_tx, control_rx) = unbounded::<ControlPacket>();
        let stream_id = StreamId::generate();

        control_tx.send(ControlPacket::Init(stream_id.clone())).await?;
        control_tx.close_channel();
        let _ = tunnel_client(client, ws_tx, control_rx).await;

        let payload = ws_rx.next().await.unwrap().into_bytes();
        let packet = ControlPacket::deserialize(&payload)?;
        assert_eq!(packet, ControlPacket::Init(stream_id.clone()));
        assert_eq!(ws_rx.next().await, None);

        Ok(())
    }
}