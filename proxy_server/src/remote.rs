use futures::channel::mpsc::UnboundedReceiver;
use futures::prelude::*;
use metrics::{increment_counter, gauge};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::oneshot;
use tracing::Instrument;
use tokio_util::sync::CancellationToken;

use crate::active_stream::{ActiveStream, ActiveStreams, StreamMessage};
use crate::connected_clients::Connections;
use crate::control_server;
pub use magic_tunnel_lib::{ClientId, ControlPacket, StreamId};

#[tracing::instrument(skip(conn, active_streams, listen_addr))]
pub async fn spawn_remote(
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    listen_addr: impl ToSocketAddrs + std::fmt::Debug + Clone,
    host: String,
) -> io::Result<CancellationToken> {
    // create our accept any server
    let listener = TcpListener::bind(listen_addr.clone()).await?;
    tracing::info!(remote = %host, "remote process listening on {:?}", listen_addr);

    let cancellation_token = CancellationToken::new();
    let ct = cancellation_token.clone();

    tokio::spawn(async move {
        loop {
            let socket = tokio::select! {
                socket = listener.accept() => {
                    match socket {
                        Ok((socket, _)) => socket,
                        _ => {
                            tracing::error!(remote = %host, "failed to accept socket");
                            continue;
                        }
                    }
                },
                _ = ct.cancelled() => {
                    tracing::info!(remote = %host, "tcp listener is cancelled.");
                    return;
                },
            };

            let host_clone = host.clone();
            tokio::spawn(
                async move {
                    accept_connection(conn, active_streams.clone(), socket, host_clone).await;
                }
                .instrument(tracing::info_span!("remote_connect")),
            );
        }
    }.instrument(tracing::info_span!("spawn_accept_connection")));
    Ok(cancellation_token)
}

// stream has selected because each stream listen different port
#[tracing::instrument(skip(conn, active_streams, socket))]
pub async fn accept_connection(
    conn: &'static Connections,
    active_streams: ActiveStreams,
    mut socket: TcpStream,
    host: String,
) {
    tracing::info!(remote = %host, "new remote connection");
    increment_counter!("magic_tunnel_server.remotes.success");

    // find the client listening for this host
    let client = match Connections::find_by_host(conn, &host) {
        Some(client) => client.clone(),
        None => {
            tracing::error!(remote = %host, "failed to find instance");
            let _ = socket.write_all(HTTP_ERROR_LOCATING_HOST_RESPONSE).await;
            return;
        }
    };
    let client_id = client.id.clone();

    // allocate a new stream for this request
    let (active_stream, queue_rx) = ActiveStream::new(client.clone());
    let stream_id = active_stream.id.clone();

    tracing::info!(remote = %host, cid = %client_id, sid = %active_stream.id.to_string(), "new stream connected");
    let (stream, sink) = tokio::io::split(socket);

    // add our stream
    active_streams.insert(stream_id.clone(), active_stream.clone());
    gauge!("magic_tunnel_server.remotes.streams", active_streams.len() as f64);
    tracing::info!(remote = %host, cid = %client_id, sid = %active_stream.id.to_string(), "register stream to active_streams len={}", active_streams.len());

    // read from socket, write to client
    let active_streams_clone = active_streams.clone();
    let stream_id_clone = stream_id.clone();
    let client_id_clone = client_id.clone();
    tokio::spawn(
        async move {
            let active_streams = active_streams_clone;
            let stream_id = stream_id_clone;
            let client_id = client_id_clone;

            process_tcp_stream(conn, active_stream, stream).await;
            active_streams.remove(&stream_id);
            gauge!("magic_tunnel_server.remotes.streams", active_streams.len() as f64);
            tracing::debug!(cid = %client_id, sid = %stream_id.to_string(), "remove stream from active_streams, process_tcp_stream len={}", active_streams.len());
        }
        .instrument(tracing::info_span!("process_tcp_stream")),
    );

    // read from client, write to socket
    tokio::spawn(
        async move {
            let reason = tunnel_to_stream(host.clone(), stream_id.clone(), sink, queue_rx).await;
            tracing::debug!(
                remote = %host, cid = %client_id, sid = %stream_id.to_string(), "tunnel_to_stream closed with reason: {:?}", reason
            );
            active_streams.remove(&stream_id);
            gauge!("magic_tunnel_server.remotes.streams", active_streams.len() as f64);
            tracing::debug!(cid = %client_id, sid = %stream_id.to_string(), "remove stream from active_streams, tunnel_to_stream len={}", active_streams.len());
        }
        .instrument(tracing::info_span!("tunnel_to_stream")),
    );
}

pub const HTTP_ERROR_LOCATING_HOST_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 27\r\n\r\nError: Error finding tunnel";
pub const HTTP_NOT_FOUND_RESPONSE: &'static [u8] =
    b"HTTP/1.1 404\r\nContent-Length: 23\r\n\r\nError: Tunnel Not Found";
pub const HTTP_TUNNEL_REFUSED_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 32\r\n\r\nTunnel says: connection refused.";

/// Process Messages from the control path in & out of the remote stream
#[tracing::instrument(skip(tunnel_stream, tcp_stream, conn))]
// async fn process_tcp_stream(conn: &Connections, mut tunnel_stream: ActiveStream, mut tcp_stream: ReadHalf<TcpStream>) {
async fn process_tcp_stream<T>(
    conn: &Connections,
    mut tunnel_stream: ActiveStream,
    mut tcp_stream: T,
) where
    T: AsyncRead + Unpin,
{
    let client_id = tunnel_stream.client.id.clone();
    let stream_id = tunnel_stream.id.clone();
    // send initial control stream init to client
    if let Err(e) = control_server::send_client_stream_init(tunnel_stream.clone()).await {
        tracing::error!(cid = %client_id, sid = %stream_id.to_string(), "failed to send stream init: {:?}", e);
        return;
    }
    tracing::debug!(cid = %client_id, sid = %stream_id.to_string(), "send stream init");

    // now read from stream and forward to clients
    let mut buf = [0; 1024];

    loop {
        // client is no longer connected
        if Connections::get(conn, &tunnel_stream.client.id).is_none() {
            tracing::debug!(cid = %client_id, sid = %stream_id.to_string(), "client disconnected, closing stream");

            // close remote-writer channel
            let _ = tunnel_stream.tx.send(StreamMessage::NoClientTunnel).await;
            tunnel_stream.tx.close_channel();
            return;
        }

        // read from stream
        let n = match tcp_stream.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(cid = %client_id, sid = %stream_id.to_string(), "failed to read from tcp socket: {:?}", e);
                return;
            }
        };

        if n == 0 {
            tracing::debug!(cid = %client_id, sid = %stream_id.to_string(), "remote client streams end");
            let _ = tunnel_stream
                .client
                .tx
                .send(ControlPacket::End(tunnel_stream.id.clone()))
                .await
                .map_err(|e| {
                    tracing::error!(cid = %client_id, sid = %stream_id.to_string(), "failed to send end signal: {:?}", e);
                });
            return;
        }

        tracing::debug!(cid = %client_id, sid = %stream_id.to_string(), "read {} bytes message from remote client", n);

        if tunnel_stream.tx.is_closed() {
            tracing::debug!(cid = %client_id, sid = %stream_id.to_string(), "process_tcp_stream closed because active_stream.tx has closed");
            return;
        }

        let data = &buf[..n];
        let packet = ControlPacket::Data(tunnel_stream.id.clone(), data.to_vec());

        match tunnel_stream.client.tx.send(packet.clone()).await {
            Ok(_) => tracing::debug!(cid = %client_id, sid = %stream_id.to_string(), "sent data packet to client"),
            Err(_) => {
                // TODO: not tested
                // This line extecuted when
                // - Corresponding client not found or closed
                tracing::error!(cid = %client_id, sid = %stream_id.to_string(), "failed to forward tcp packets to disconnected client. dropping client.");
                Connections::remove(conn, &tunnel_stream.client);
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

// queue is automatically closed when queue is dropped at the end of this function
#[tracing::instrument(skip(sink, stream_id, queue))]
async fn tunnel_to_stream<T>(
    subdomain: String,
    stream_id: StreamId,
    // mut sink: WriteHalf<TcpStream>,
    mut sink: T,
    mut queue: UnboundedReceiver<StreamMessage>,
) -> TunnelToStreamExitReason
where
    T: AsyncWrite + AsyncWriteExt + Unpin,
{
    loop {
        let result = queue.next().await;

        let result = if let Some(message) = result {
            match message {
                StreamMessage::Data(data) => Some(data),
                StreamMessage::TunnelRefused => {
                    tracing::debug!(remote = %subdomain, sid = %stream_id.to_string(), "tunnel refused");
                    let _ = sink.write_all(HTTP_TUNNEL_REFUSED_RESPONSE).await;
                    None
                }
                StreamMessage::NoClientTunnel => {
                    tracing::info!(remote = %subdomain, sid = %stream_id.to_string(), "client tunnel not found");
                    let _ = sink.write_all(HTTP_NOT_FOUND_RESPONSE).await;
                    None
                }
            }
        } else {
            None
        };

        let data = match result {
            Some(data) => data,
            None => {
                tracing::debug!(remote = %subdomain, sid = %stream_id.to_string(), "done tunneling to sink");
                let _ = sink.shutdown().await.map_err(|_e| {
                    tracing::error!(remote = %subdomain, sid = %stream_id.to_string(), "error shutting down tcp stream");
                });

                // active_streams.remove(&stream_id);
                // return;
                return TunnelToStreamExitReason::QueueClosed;
            }
        };

        let result = sink.write_all(&data).await;

        if let Some(error) = result.err() {
            tracing::warn!(remote = %subdomain, sid = %stream_id.to_string(), "stream closed, disconnecting: {:?}", error);
            return TunnelToStreamExitReason::TcpClosed;
        }
    }
}

#[cfg(test)]
mod process_tcp_stream_test {
    use super::*;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use tokio_test::io::Builder;

    use std::io;
    use std::pin::Pin;
    use std::task::{self, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    use crate::connected_clients::ConnectedClient;

    struct InfiniteRead {}
    impl InfiniteRead {
        fn new() -> Self {
            InfiniteRead {}
        }
    }
    impl AsyncRead for InfiniteRead {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            buf.put_slice(b"infinite source");
            return Poll::Ready(Ok(()));
        }
    }

    fn create_active_stream() -> (
        Connections,
        ConnectedClient,
        ActiveStream,
        UnboundedReceiver<StreamMessage>,
        UnboundedReceiver<ControlPacket>,
    ) {
        let conn = Connections::new();
        let (tx, rx) = unbounded::<ControlPacket>();
        let client = ConnectedClient {
            id: ClientId::generate(),
            host: "foobar".into(),
            tx,
        };

        let (active_stream, stream_rx) = ActiveStream::new(client.clone());

        (conn, client, active_stream, stream_rx, rx)
    }

    #[tokio::test]
    async fn send_stream_init() -> Result<(), Box<dyn std::error::Error>> {
        let (conn, _client, active_stream, _stream_rx, mut client_rx) = create_active_stream();

        let tcp_mock = Builder::new().build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert!(matches!(
            client_rx.next().await.unwrap(),
            ControlPacket::Init(_)
        ));
        Ok(())
    }
    #[tokio::test]
    async fn send_noclienttunnel_to_remote_when_client_not_registered(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (conn, _client, active_stream, mut stream_rx, _client_rx) = create_active_stream();

        let tcp_mock = Builder::new().build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(
            stream_rx.next().await.unwrap(),
            StreamMessage::NoClientTunnel
        );
        Ok(())
    }

    #[tokio::test]
    async fn send_stream_end() -> Result<(), Box<dyn std::error::Error>> {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        Connections::add(&conn, client.clone());
        let stream_id = active_stream.id.clone();

        let tcp_mock = Builder::new().build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::Init(stream_id.clone())
        );
        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::End(stream_id.clone())
        );

        Ok(())
    }

    #[tokio::test]
    async fn client_stream_should_close_when_client_is_dropped(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        Connections::add(&conn, client.clone());

        let tcp_mock = Builder::new().build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert!(client_rx.next().await.is_some());
        assert!(client_rx.next().await.is_some());

        // ensure client_tx is dropped
        Connections::remove(&conn, &client);
        drop(client);
        assert_eq!(client_rx.next().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn forward_data() -> Result<(), Box<dyn std::error::Error>> {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        Connections::add(&conn, client);
        let stream_id = active_stream.id.clone();

        let tcp_mock = Builder::new().read(b"foobar").build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::Init(stream_id.clone())
        );
        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::Data(stream_id.clone(), b"foobar".to_vec())
        );
        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::End(stream_id.clone())
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_to_send_stream_init_when_client_closed(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        client.tx.close_channel();
        Connections::add(&conn, client);

        let tcp_mock = Builder::new().build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(client_rx.next().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn stop_wait_read_when_tunnel_stream_is_closed() -> Result<(), Box<dyn std::error::Error>>
    {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        Connections::add(&conn, client);

        client_rx.close();
        let infinite_reader = InfiniteRead::new();
        let _ = process_tcp_stream(&conn, active_stream, infinite_reader).await;

        Ok(())
    }
}

#[cfg(test)]
mod tunnel_to_stream_test {
    use super::*;
    use futures::channel::mpsc::unbounded;
    use std::io;
    use tokio_test::io::Builder;

    #[tokio::test]
    async fn mock_test() -> Result<(), Box<dyn std::error::Error>> {
        let mut tcp_mock = Builder::new().write(b"foobarbaz").write(b"piyo").build();

        // writed data must be exactly same as builder args
        // any grouping is accepted because of stream
        tcp_mock.write(b"foobar").await?;
        tcp_mock.write(b"bazpiyo").await?;
        Ok(())
    }
    #[tokio::test]
    async fn must_exit_from_loop_when_tcp_raises_error() -> Result<(), Box<dyn std::error::Error>> {
        let error = io::Error::new(io::ErrorKind::Other, "cruel");
        let tcp_mock = Builder::new().write_error(error).build();
        let (mut tx, rx) = unbounded();

        tx.send(StreamMessage::Data(b"foobar".to_vec())).await?;
        let reason =
            tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;

        assert_eq!(reason, TunnelToStreamExitReason::TcpClosed);
        assert_eq!(tx.is_closed(), true);
        Ok(())
    }

    #[tokio::test]
    async fn tcp_stream_must_shutdown_when_queue_is_closed(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let tcp_mock = Builder::new().build();
        let (tx, rx) = unbounded();
        tx.close_channel();

        let reason =
            tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;
        assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        Ok(())
    }

    #[tokio::test]
    async fn tcp_stream_must_shutdown_when_tunnel_refused() -> Result<(), Box<dyn std::error::Error>>
    {
        let tcp_mock = Builder::new().build();
        let (mut tx, rx) = unbounded();

        tx.send(StreamMessage::TunnelRefused).await?;
        let reason =
            tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;

        assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        assert_eq!(tx.is_closed(), true);
        Ok(())
    }

    #[tokio::test]
    async fn tcp_stream_must_shutdown_when_no_client_tunnel(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let tcp_mock = Builder::new().build();
        let (mut tx, rx) = unbounded();

        tx.send(StreamMessage::NoClientTunnel).await?;
        let reason =
            tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;

        assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        assert_eq!(tx.is_closed(), true);
        Ok(())
    }

    #[tokio::test]
    async fn forward_data() -> Result<(), Box<dyn std::error::Error>> {
        let tcp_mock = Builder::new().write(b"foobarbaz").build();
        let (mut tx, rx) = unbounded();

        tx.send(StreamMessage::Data(b"foobarbaz".to_vec())).await?;
        tx.close_channel();
        let reason =
            tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;

        assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        Ok(())
    }
}

#[cfg(test)]
mod spawn_remote_test {
    use super::*;
    use crate::connected_clients::ConnectedClient;
    use dashmap::DashMap;
    use futures::channel::mpsc::unbounded;
    use lazy_static::lazy_static;
    use std::io;
    use std::sync::Arc;
    use std::time::Duration;

    async fn launch_remote(remote_port: u16) -> io::Result<CancellationToken> {
        lazy_static! {
            pub static ref CONNECTIONS: Connections = Connections::new();
            pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(DashMap::new());
        }
        // we must clear CONNECTIONS, ACTIVE_STREAMS
        // because they are shared across test
        Connections::clear(&CONNECTIONS);
        ACTIVE_STREAMS.clear();

        let (tx, _rx) = unbounded::<ControlPacket>();
        let client = ConnectedClient {
            id: ClientId::generate(),
            host: "host-aaa".to_string(),
            tx,
        };
        Connections::add(&CONNECTIONS, client.clone());

        spawn_remote(
            &CONNECTIONS,
            &ACTIVE_STREAMS,
            format!("[::]:{}", remote_port),
            "host-aaa".to_string(),
        )
        .await
    }

    #[tokio::test]
    async fn accept_remote_connection() -> Result<(), Box<dyn std::error::Error>> {
        let remote_port = 5678;
        let _remote_cancel_handler = launch_remote(remote_port).await?;

        let _ = TcpStream::connect(format!("127.0.0.1:{}", remote_port))
            .await
            .expect("Failed to connect to remote port");
        Ok(())
    }

    #[tokio::test]
    async fn reject_remote_connection_after_cancellation() -> Result<(), Box<dyn std::error::Error>>
    {
        let remote_port = 5679;
        let remote_cancel_handler = launch_remote(remote_port).await?;

        let _ = TcpStream::connect(format!("127.0.0.1:{}", remote_port))
            .await
            .expect("Failed to connect to remote port");
        remote_cancel_handler.cancel();
        tokio::time::sleep(Duration::from_secs(3)).await;

        let remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await;
        assert!(remote.is_err());

        Ok(())
    }
}
