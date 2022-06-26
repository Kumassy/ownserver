use futures::channel::mpsc::UnboundedReceiver;
use futures::prelude::*;
use metrics::{increment_counter, gauge};
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use tokio::sync::oneshot;
use tracing::Instrument;
use tokio_util::sync::CancellationToken;
use std::sync::Arc;

use crate::active_stream::{ActiveStream, ActiveStreams, StreamMessage};
use crate::connected_clients::Connections;
use crate::control_server;
pub use magic_tunnel_lib::{ClientId, ControlPacket, StreamId};

// #[tracing::instrument(skip(conn, active_streams, listen_addr))]
pub async fn spawn_remote(
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    listen_addr: impl ToSocketAddrs + std::fmt::Debug + Clone,
    host: String,
) -> io::Result<CancellationToken> {
    let socket = UdpSocket::bind(listen_addr.clone()).await?;
    tracing::info!(remote = %host, "remote process listening on {:?}", listen_addr);
    let socket = Arc::new(socket);

    let cancellation_token = CancellationToken::new();
    let ct = cancellation_token.clone();

    // read from socket, write to client
    let active_streams_clone = active_streams.clone();

    tokio::spawn(
        async move {
            let active_streams = active_streams_clone;
            process_udp_stream(ct, conn, active_streams, host, socket).await;
        }
        .instrument(tracing::info_span!("process_udp_stream")),
    );

    Ok(cancellation_token)
}

pub const HTTP_ERROR_LOCATING_HOST_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 27\r\n\r\nError: Error finding tunnel";
pub const HTTP_NOT_FOUND_RESPONSE: &'static [u8] =
    b"HTTP/1.1 404\r\nContent-Length: 23\r\n\r\nError: Tunnel Not Found";
pub const HTTP_TUNNEL_REFUSED_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 32\r\n\r\nTunnel says: connection refused.";

#[tracing::instrument(skip(ct, conn, active_streams, udp_socket))]
async fn process_udp_stream(
    ct: CancellationToken,
    conn: &Connections,
    mut active_streams: ActiveStreams,
    host: String,
    udp_socket: Arc<UdpSocket>,
)
{
    let mut buf = [0; 2048];

    // find the client listening for this host
    let client_id = match Connections::find_by_host(conn, &host) {
        Some(client) => client.id,
        None => {
            tracing::error!(remote = %host, "failed to find instance");
            return;
        }
    };

    loop {
        // client is no longer connected
        if Connections::get(conn, &client_id).is_none() {
            tracing::debug!(cid = %client_id, "client disconnected, closing stream");
            return;
        }

        // TODO: cancellation token
        // read from stream
        let (n, peer_addr) = match udp_socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(cid = %client_id, "failed to read from tcp socket: {:?}", e);
                return;
            }
        };

        let mut active_stream = match active_streams.find_by_addr(&peer_addr) {
            Some(active_stream) => active_stream.clone(),
            None => {
                let client = match Connections::get(conn, &client_id) {
                    Some(client) => client.clone(),
                    None => {
                        tracing::error!(client_id = %client_id, "failed to find client");
                        return;
                    }
                };

                let (active_stream, queue_rx) = ActiveStream::new(client.clone());
                let stream_id = active_stream.id;
                let active_streams_ = active_streams.clone();
                let udp_socket_ = udp_socket.clone();
                let ct_ = ct.clone();

                tokio::spawn(async move {
                    let reason = tunnel_to_stream(ct_, queue_rx, udp_socket_, peer_addr).await;

                    tracing::debug!(
                        cid = %client_id, sid = %stream_id, "tunnel_to_stream closed with reason: {:?}", reason
                    );
                    active_streams_.remove(&stream_id);
                }.instrument(tracing::info_span!("spawn_tunnel_to_stream")));

                active_streams.insert(stream_id, active_stream.clone(), peer_addr);
                active_stream
            }
        };
        gauge!("magic_tunnel_server.remotes.udp.streams", active_streams.len() as f64);
        let stream_id = active_stream.id;

        if n == 0 {
            tracing::debug!(cid = %client_id, sid = %stream_id, "remote client streams end");
            let _ = active_stream
                .client
                .tx
                .send(ControlPacket::End(stream_id))
                .await
                .map_err(|e| {
                    tracing::error!(cid = %client_id, sid = %stream_id, "failed to send end signal: {:?}", e);
                });
            return;
        }

        tracing::debug!(cid = %client_id, sid = %stream_id, "read {} bytes message from remote client", n);

        if active_stream.tx.is_closed() {
            tracing::debug!(cid = %client_id, sid = %stream_id, "process_tcp_stream closed because active_stream.tx has closed");
            return;
        }

        let data = &buf[..n];
        let packet = ControlPacket::UdpData(stream_id, data.to_vec());

        match active_stream.client.tx.send(packet.clone()).await {
            Ok(_) => tracing::debug!(cid = %client_id, sid = %stream_id, "sent data packet to client"),
            Err(_) => {
                // TODO: not tested
                // This line extecuted when
                // - Corresponding client not found or closed
                tracing::error!(cid = %client_id, sid = %stream_id, "failed to forward tcp packets to disconnected client. dropping client.");
                Connections::remove(conn, &active_stream.client);
                tracing::debug!(cid = %client_id, "remove client from connections len_clients={} len_hosts={}", Connections::len_clients(conn), Connections::len_hosts(conn));
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum TunnelToStreamExitReason {
    QueueClosed,
    TcpClosed,
}

#[tracing::instrument(skip(ct, queue, socket))]
async fn tunnel_to_stream(
    ct: CancellationToken,
    mut queue: UnboundedReceiver<StreamMessage>,
    socket: Arc<UdpSocket>,
    peer_addr: SocketAddr,
) -> TunnelToStreamExitReason
{
    loop {
        // TODO: cancellation token
        let result = queue.next().await;

        let result = if let Some(message) = result {
            match message {
                StreamMessage::Data(data) => Some(data),
                StreamMessage::TunnelRefused => {
                    tracing::debug!("tunnel refused");
                    let _ = socket.send_to(HTTP_TUNNEL_REFUSED_RESPONSE, peer_addr).await;
                    None
                }
                StreamMessage::NoClientTunnel => {
                    tracing::info!("client tunnel not found");
                    let _ = socket.send_to(HTTP_NOT_FOUND_RESPONSE, peer_addr).await;
                    None
                }
            }
        } else {
            None
        };

        let data = match result {
            Some(data) => data,
            None => {
                tracing::debug!("done tunneling to sink");
                return TunnelToStreamExitReason::QueueClosed;
            }
        };

        let result = socket.send_to(&data, peer_addr).await;

        if let Some(error) = result.err() {
            tracing::warn!("stream closed, disconnecting: {:?}", error);
            return TunnelToStreamExitReason::TcpClosed;
        }
    }
}

// #[cfg(test)]
// mod process_udp_stream_test {
//     use super::*;
//     use futures::channel::mpsc::{unbounded, UnboundedReceiver};
//     use tokio_test::io::Builder;

//     use std::io;
//     use std::pin::Pin;
//     use std::task::{self, Poll};
//     use tokio::io::{AsyncRead, ReadBuf};

//     use crate::connected_clients::ConnectedClient;

//     struct InfiniteRead {}
//     impl InfiniteRead {
//         fn new() -> Self {
//             InfiniteRead {}
//         }
//     }
//     impl AsyncRead for InfiniteRead {
//         fn poll_read(
//             self: Pin<&mut Self>,
//             _cx: &mut task::Context<'_>,
//             buf: &mut ReadBuf<'_>,
//         ) -> Poll<io::Result<()>> {
//             buf.put_slice(b"infinite source");
//             return Poll::Ready(Ok(()));
//         }
//     }

//     fn create_active_stream() -> (
//         Connections,
//         ConnectedClient,
//         ActiveStream,
//         UnboundedReceiver<StreamMessage>,
//         UnboundedReceiver<ControlPacket>,
//     ) {
//         let conn = Connections::new();
//         let (tx, rx) = unbounded::<ControlPacket>();
//         let client = ConnectedClient {
//             id: ClientId::generate(),
//             host: "foobar".into(),
//             tx,
//         };

//         let (active_stream, stream_rx) = ActiveStream::new(client.clone());

//         (conn, client, active_stream, stream_rx, rx)
//     }

//     #[tokio::test]
//     async fn send_stream_init() -> Result<(), Box<dyn std::error::Error>> {
//         let (conn, _client, active_stream, _stream_rx, mut client_rx) = create_active_stream();

//         let tcp_mock = Builder::new().build();
//         let _ = process_udp_stream(&conn, active_stream, tcp_mock).await;

//         assert!(matches!(
//             client_rx.next().await.unwrap(),
//             ControlPacket::Init(_)
//         ));
//         Ok(())
//     }
//     #[tokio::test]
//     async fn send_noclienttunnel_to_remote_when_client_not_registered(
//     ) -> Result<(), Box<dyn std::error::Error>> {
//         let (conn, _client, active_stream, mut stream_rx, _client_rx) = create_active_stream();

//         let tcp_mock = Builder::new().build();
//         let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

//         assert_eq!(
//             stream_rx.next().await.unwrap(),
//             StreamMessage::NoClientTunnel
//         );
//         Ok(())
//     }

//     #[tokio::test]
//     async fn send_stream_end() -> Result<(), Box<dyn std::error::Error>> {
//         let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
//         Connections::add(&conn, client.clone());
//         let stream_id = active_stream.id.clone();

//         let tcp_mock = Builder::new().build();
//         let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

//         assert_eq!(
//             client_rx.next().await.unwrap(),
//             ControlPacket::Init(stream_id.clone())
//         );
//         assert_eq!(
//             client_rx.next().await.unwrap(),
//             ControlPacket::End(stream_id.clone())
//         );

//         Ok(())
//     }

//     #[tokio::test]
//     async fn client_stream_should_close_when_client_is_dropped(
//     ) -> Result<(), Box<dyn std::error::Error>> {
//         let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
//         Connections::add(&conn, client.clone());

//         let tcp_mock = Builder::new().build();
//         let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

//         assert!(client_rx.next().await.is_some());
//         assert!(client_rx.next().await.is_some());

//         // ensure client_tx is dropped
//         Connections::remove(&conn, &client);
//         drop(client);
//         assert_eq!(client_rx.next().await, None);
//         Ok(())
//     }

//     #[tokio::test]
//     async fn forward_data() -> Result<(), Box<dyn std::error::Error>> {
//         let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
//         Connections::add(&conn, client);
//         let stream_id = active_stream.id.clone();

//         let tcp_mock = Builder::new().read(b"foobar").build();
//         let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

//         assert_eq!(
//             client_rx.next().await.unwrap(),
//             ControlPacket::Init(stream_id.clone())
//         );
//         assert_eq!(
//             client_rx.next().await.unwrap(),
//             ControlPacket::Data(stream_id.clone(), b"foobar".to_vec())
//         );
//         assert_eq!(
//             client_rx.next().await.unwrap(),
//             ControlPacket::End(stream_id.clone())
//         );
//         Ok(())
//     }

//     #[tokio::test]
//     async fn failed_to_send_stream_init_when_client_closed(
//     ) -> Result<(), Box<dyn std::error::Error>> {
//         let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
//         client.tx.close_channel();
//         Connections::add(&conn, client);

//         let tcp_mock = Builder::new().build();
//         let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

//         assert_eq!(client_rx.next().await, None);
//         Ok(())
//     }

//     #[tokio::test]
//     async fn stop_wait_read_when_tunnel_stream_is_closed() -> Result<(), Box<dyn std::error::Error>>
//     {
//         let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
//         Connections::add(&conn, client);

//         client_rx.close();
//         let infinite_reader = InfiniteRead::new();
//         let _ = process_tcp_stream(&conn, active_stream, infinite_reader).await;

//         Ok(())
//     }
// }

// #[cfg(test)]
// mod tunnel_to_stream_test {
//     use super::*;
//     use futures::channel::mpsc::unbounded;
//     use std::io;
//     use tokio_test::io::Builder;

//     #[tokio::test]
//     async fn mock_test() -> Result<(), Box<dyn std::error::Error>> {
//         let mut tcp_mock = Builder::new().write(b"foobarbaz").write(b"piyo").build();

//         // writed data must be exactly same as builder args
//         // any grouping is accepted because of stream
//         tcp_mock.write(b"foobar").await?;
//         tcp_mock.write(b"bazpiyo").await?;
//         Ok(())
//     }
//     #[tokio::test]
//     async fn must_exit_from_loop_when_tcp_raises_error() -> Result<(), Box<dyn std::error::Error>> {
//         let error = io::Error::new(io::ErrorKind::Other, "cruel");
//         let tcp_mock = Builder::new().write_error(error).build();
//         let (mut tx, rx) = unbounded();

//         tx.send(StreamMessage::Data(b"foobar".to_vec())).await?;
//         let reason =
//             tunnel_to_stream("foobar", StreamId::generate(), tcp_mock, rx).await;

//         assert_eq!(reason, TunnelToStreamExitReason::TcpClosed);
//         assert_eq!(tx.is_closed(), true);
//         Ok(())
//     }

//     #[tokio::test]
//     async fn tcp_stream_must_shutdown_when_queue_is_closed(
//     ) -> Result<(), Box<dyn std::error::Error>> {
//         let tcp_mock = Builder::new().build();
//         let (tx, rx) = unbounded();
//         tx.close_channel();

//         let reason =
//             tunnel_to_stream("foobar", StreamId::generate(), tcp_mock, rx).await;
//         assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
//         Ok(())
//     }

//     #[tokio::test]
//     async fn tcp_stream_must_shutdown_when_tunnel_refused() -> Result<(), Box<dyn std::error::Error>>
//     {
//         let tcp_mock = Builder::new().build();
//         let (mut tx, rx) = unbounded();

//         tx.send(StreamMessage::TunnelRefused).await?;
//         let reason =
//             tunnel_to_stream("foobar", StreamId::generate(), tcp_mock, rx).await;

//         assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
//         assert_eq!(tx.is_closed(), true);
//         Ok(())
//     }

//     #[tokio::test]
//     async fn tcp_stream_must_shutdown_when_no_client_tunnel(
//     ) -> Result<(), Box<dyn std::error::Error>> {
//         let tcp_mock = Builder::new().build();
//         let (mut tx, rx) = unbounded();

//         tx.send(StreamMessage::NoClientTunnel).await?;
//         let reason =
//             tunnel_to_stream("foobar", StreamId::generate(), tcp_mock, rx).await;

//         assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
//         assert_eq!(tx.is_closed(), true);
//         Ok(())
//     }

//     #[tokio::test]
//     async fn forward_data() -> Result<(), Box<dyn std::error::Error>> {
//         let tcp_mock = Builder::new().write(b"foobarbaz").build();
//         let (mut tx, rx) = unbounded();

//         tx.send(StreamMessage::Data(b"foobarbaz".to_vec())).await?;
//         tx.close_channel();
//         let reason =
//             tunnel_to_stream("foobar", StreamId::generate(), tcp_mock, rx).await;

//         assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
//         Ok(())
//     }
// }

// #[cfg(test)]
// mod spawn_remote_test {
//     use super::*;
//     use crate::connected_clients::ConnectedClient;
//     use dashmap::DashMap;
//     use futures::channel::mpsc::unbounded;
//     use lazy_static::lazy_static;
//     use std::io;
//     use std::sync::Arc;
//     use std::time::Duration;

//     async fn launch_remote(remote_port: u16) -> io::Result<CancellationToken> {
//         lazy_static! {
//             pub static ref CONNECTIONS: Connections = Connections::new();
//             pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(DashMap::new());
//         }
//         // we must clear CONNECTIONS, ACTIVE_STREAMS
//         // because they are shared across test
//         Connections::clear(&CONNECTIONS);
//         ACTIVE_STREAMS.clear();

//         let (tx, _rx) = unbounded::<ControlPacket>();
//         let client = ConnectedClient {
//             id: ClientId::generate(),
//             host: "host-aaa",
//             tx,
//         };
//         Connections::add(&CONNECTIONS, client.clone());

//         spawn_remote(
//             &CONNECTIONS,
//             &ACTIVE_STREAMS,
//             format!("[::]:{}", remote_port),
//             "host-aaa",
//         )
//         .await
//     }

//     #[tokio::test]
//     async fn accept_remote_connection() -> Result<(), Box<dyn std::error::Error>> {
//         let remote_port = 5678;
//         let _remote_cancel_handler = launch_remote(remote_port).await?;

//         let _ = TcpStream::connect(format!("127.0.0.1:{}", remote_port))
//             .await
//             .expect("Failed to connect to remote port");
//         Ok(())
//     }

//     #[tokio::test]
//     async fn reject_remote_connection_after_cancellation() -> Result<(), Box<dyn std::error::Error>>
//     {
//         let remote_port = 5679;
//         let remote_cancel_handler = launch_remote(remote_port).await?;

//         let _ = TcpStream::connect(format!("127.0.0.1:{}", remote_port))
//             .await
//             .expect("Failed to connect to remote port");
//         remote_cancel_handler.cancel();
//         tokio::time::sleep(Duration::from_secs(3)).await;

//         let remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await;
//         assert!(remote.is_err());

//         Ok(())
//     }
// }
