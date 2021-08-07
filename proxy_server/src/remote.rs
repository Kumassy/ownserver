use futures::channel::mpsc::UnboundedReceiver;
use futures::prelude::*;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::oneshot;
use tracing::Instrument;

use crate::active_stream::{ActiveStream, ActiveStreams, StreamMessage};
use crate::connected_clients::Connections;
use crate::control_server;
pub use magic_tunnel_lib::{ClientId, ControlPacket, StreamId};

pub type CancelHander = oneshot::Sender<()>;
pub async fn spawn_remote(
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    listen_addr: impl ToSocketAddrs + std::fmt::Debug + Clone,
    host: String,
) -> io::Result<CancelHander> {
    // create our accept any server
    let listener = TcpListener::bind(listen_addr.clone()).await?;
    tracing::info!("remote={} remote process listening on {:?}", host, listen_addr);

    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            let socket = tokio::select! {
                socket = listener.accept() => {
                    match socket {
                        Ok((socket, _)) => socket,
                        _ => {
                            tracing::error!("remote={} failed to accept socket", host);
                            continue;
                        }
                    }
                },
                _ = (&mut cancel_rx) => {
                    tracing::info!("remote={} tcp listener is cancelled.", host);
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
    });
    Ok(cancel_tx)
}

// stream has selected because each stream listen different port
#[tracing::instrument(skip(socket, conn, active_streams, host))]
pub async fn accept_connection(
    conn: &'static Connections,
    active_streams: ActiveStreams,
    mut socket: TcpStream,
    host: String,
) {
    tracing::info!("remote={} new remote connection", host);

    // find the client listening for this host
    let client = match Connections::find_by_host(conn, &host) {
        Some(client) => client.clone(),
        None => {
            tracing::error!("remote={} failed to find instance", host);
            let _ = socket.write_all(HTTP_ERROR_LOCATING_HOST_RESPONSE).await;
            return;
        }
    };
    let client_id = client.id.clone();

    // allocate a new stream for this request
    let (active_stream, queue_rx) = ActiveStream::new(client.clone());
    let stream_id = active_stream.id.clone();

    tracing::info!("remote={} cid={} sid={} new stream connected", host, client_id, active_stream.id.to_string());
    let (stream, sink) = tokio::io::split(socket);

    // add our stream
    active_streams.insert(stream_id.clone(), active_stream.clone());
    tracing::debug!("remote={} cid={} sid={} register stream to active_streams len={}", host, client_id, stream_id.to_string(), active_streams.len());


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
            tracing::debug!("cid={} sid={} remove stream from active_streams, process_tcp_stream len={}", client_id, stream_id.to_string(), active_streams.len());
        }
        .instrument(tracing::info_span!("process_tcp_stream")),
    );

    // read from client, write to socket
    tokio::spawn(
        async move {
            let reason = tunnel_to_stream(host.clone(), stream_id.clone(), sink, queue_rx).await;
            tracing::debug!(
                "remote={} cid={} sid={} tunnel_to_stream closed with reason: {:?}", host, client_id, stream_id.to_string(), reason
            );
            active_streams.remove(&stream_id);
            tracing::debug!("cid={} sid={} remove stream from active_streams, tunnel_to_stream len={}", client_id, stream_id.to_string(), active_streams.len());
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
        tracing::error!("cid={} sid={} failed to send stream init: {:?}", client_id, stream_id.to_string(), e);
        return;
    }
    tracing::debug!("cid={} sid={} send stream init", client_id, stream_id.to_string());

    // now read from stream and forward to clients
    let mut buf = [0; 1024];

    loop {
        // client is no longer connected
        if Connections::get(conn, &tunnel_stream.client.id).is_none() {
            tracing::debug!("cid={} sid={} client disconnected, closing stream", client_id, stream_id.to_string());

            // close remote-writer channel
            let _ = tunnel_stream.tx.send(StreamMessage::NoClientTunnel).await;
            tunnel_stream.tx.close_channel();
            return;
        }

        // read from stream
        let n = match tcp_stream.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("cid={} sid={} failed to read from tcp socket: {:?}", client_id, stream_id.to_string(), e);
                return;
            }
        };

        if n == 0 {
            tracing::debug!("cid={} sid={} remote client streams end", client_id, stream_id.to_string());
            let _ = tunnel_stream
                .client
                .tx
                .send(ControlPacket::End(tunnel_stream.id.clone()))
                .await
                .map_err(|e| {
                    tracing::error!("cid={} sid={} failed to send end signal: {:?}", client_id, stream_id.to_string(), e);
                });
            return;
        }

        tracing::debug!("cid={} sid={} read {} bytes message from remote client", client_id, stream_id.to_string(), n);

        if tunnel_stream.tx.is_closed() {
            tracing::debug!("cid={} sid={} process_tcp_stream closed because active_stream.tx has closed", client_id, stream_id.to_string());
            return;
        }

        let data = &buf[..n];
        let packet = ControlPacket::Data(tunnel_stream.id.clone(), data.to_vec());

        match tunnel_stream.client.tx.send(packet.clone()).await {
            Ok(_) => tracing::debug!("cid={} sid={} sent data packet to client", client_id, stream_id.to_string()),
            Err(_) => {
                // TODO: not tested
                // This line extecuted when
                // - Corresponding client not found or closed
                tracing::error!("cid={} sid={} failed to forward tcp packets to disconnected client. dropping client.", client_id, stream_id.to_string());
                Connections::remove(conn, &tunnel_stream.client);
                tracing::debug!("cid={} remove client from connections len_clients={} len_hosts={}", client_id, Connections::len_clients(conn), Connections::len_hosts(conn));
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
                    tracing::debug!("remote={} sid={} tunnel refused", subdomain, stream_id.to_string());
                    let _ = sink.write_all(HTTP_TUNNEL_REFUSED_RESPONSE).await;
                    None
                }
                StreamMessage::NoClientTunnel => {
                    tracing::info!("remote={} sid={} client tunnel not found", subdomain, stream_id.to_string());
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
                tracing::debug!("remote={} sid={} done tunneling to sink", subdomain, stream_id.to_string());
                let _ = sink.shutdown().await.map_err(|_e| {
                    tracing::error!("remote={} sid={} error shutting down tcp stream", subdomain, stream_id.to_string());
                });

                // active_streams.remove(&stream_id);
                // return;
                return TunnelToStreamExitReason::QueueClosed;
            }
        };

        let result = sink.write_all(&data).await;

        if let Some(error) = result.err() {
            tracing::warn!("remote={} sid={} stream closed, disconnecting: {:?}", subdomain, stream_id.to_string(), error);
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

    async fn launch_remote(remote_port: u16) -> io::Result<CancelHander> {
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
        remote_cancel_handler.send(()).unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;

        let remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await;
        assert!(remote.is_err());

        Ok(())
    }
}
