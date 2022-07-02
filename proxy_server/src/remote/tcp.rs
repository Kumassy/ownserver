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

    // find the client listening for this host
    let client = match Connections::find_by_host(conn, &host) {
        Some(client) => client.clone(),
        None => {
            tracing::error!(remote = %host, "failed to find instance");
            let _ = socket.write_all(HTTP_ERROR_LOCATING_HOST_RESPONSE).await;
            return;
        }
    };
    let peer_addr = match socket.peer_addr() {
        Ok(addr) => addr,
        Err(_) => {
            tracing::error!(remote = %host, "failed to find remote peer addr");
            let _ = socket.write_all(HTTP_TUNNEL_REFUSED_RESPONSE).await;
            return;
        }
    };
    increment_counter!("magic_tunnel_server.remotes.success");

    let (stream, sink) = tokio::io::split(socket);
    let client_id = client.id;

    // allocate a new stream for this request
    let active_stream = ActiveStream::build_tcp(client, sink, active_streams.clone());
    let stream_id = active_stream.id;

    tracing::info!(remote = %host, cid = %client_id, sid = %active_stream.id, "new stream connected");

    // add our stream
    active_streams.insert(stream_id, active_stream.clone(), peer_addr);
    gauge!("magic_tunnel_server.remotes.streams", active_streams.len() as f64);
    tracing::info!(remote = %host, cid = %client_id, sid = %active_stream.id, "register stream to active_streams len={}", active_streams.len());

    // read from socket, write to client
    let active_streams_clone = active_streams.clone();
    tokio::spawn(
        async move {
            let active_streams = active_streams_clone;

            process_tcp_stream(conn, active_stream, stream).await;
            active_streams.remove(&stream_id);
            gauge!("magic_tunnel_server.remotes.streams", active_streams.len() as f64);
            tracing::debug!(cid = %client_id, sid = %stream_id, "remove stream from active_streams, process_tcp_stream len={}", active_streams.len());
        }
        .instrument(tracing::info_span!("process_tcp_stream")),
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
    let client_id = tunnel_stream.client_id();
    let stream_id = tunnel_stream.id;
    // send initial control stream init to client
    if let Err(e) = control_server::send_client_stream_init(tunnel_stream.clone()).await {
        tracing::error!(cid = %client_id, sid = %stream_id, "failed to send stream init: {:?}", e);
        return;
    }
    tracing::debug!(cid = %client_id, sid = %stream_id, "send stream init");

    // now read from stream and forward to clients
    let mut buf = [0; 1024];

    loop {
        // client is no longer connected
        if Connections::get(conn, &tunnel_stream.client_id()).is_none() {
            tracing::debug!(cid = %client_id, sid = %stream_id, "client disconnected, closing stream");

            // close remote-writer channel
            let _ = tunnel_stream.send_to_remote(StreamMessage::NoClientTunnel).await;
            tunnel_stream.close_channel();
            return;
        }

        // read from stream
        let n = match tcp_stream.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(cid = %client_id, sid = %stream_id, "failed to read from tcp socket: {:?}", e);
                return;
            }
        };

        if n == 0 {
            tracing::debug!(cid = %client_id, sid = %stream_id, "remote client streams end");
            let _ = tunnel_stream
                .send_to_client(ControlPacket::End(tunnel_stream.id))
                .await
                .map_err(|e| {
                    tracing::error!(cid = %client_id, sid = %stream_id, "failed to send end signal: {:?}", e);
                });
            return;
        }

        tracing::debug!(cid = %client_id, sid = %stream_id, "read {} bytes message from remote client", n);

        if tunnel_stream.is_closed() {
            tracing::debug!(cid = %client_id, sid = %stream_id, "process_tcp_stream closed because active_stream.tx has closed");
            return;
        }

        let data = &buf[..n];
        let packet = ControlPacket::Data(tunnel_stream.id, data.to_vec());

        match tunnel_stream.send_to_client(packet.clone()).await {
            Ok(_) => tracing::debug!(cid = %client_id, sid = %stream_id, "sent data packet to client"),
            Err(_) => {
                // TODO: not tested
                // This line extecuted when
                // - Corresponding client not found or closed
                tracing::error!(cid = %client_id, sid = %stream_id, "failed to forward tcp packets to disconnected client. dropping client.");
                conn.remove_by_id(tunnel_stream.client_id());
                tracing::debug!(cid = %client_id, "remove client from connections len_clients={} len_hosts={}", Connections::len_clients(conn), Connections::len_hosts(conn));
            }
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
        let client = ConnectedClient::new(ClientId::new(), "foobar".into(), tx);

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
        let stream_id = active_stream.id;

        let tcp_mock = Builder::new().build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::Init(stream_id)
        );
        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::End(stream_id)
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
        let stream_id = active_stream.id;

        let tcp_mock = Builder::new().read(b"foobar").build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::Init(stream_id)
        );
        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::Data(stream_id, b"foobar".to_vec())
        );
        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::End(stream_id)
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_to_send_stream_init_when_client_closed(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        client.close_channel();
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
            pub static ref ACTIVE_STREAMS: ActiveStreams = ActiveStreams::default();
        }
        // we must clear CONNECTIONS, ACTIVE_STREAMS
        // because they are shared across test
        Connections::clear(&CONNECTIONS);
        ACTIVE_STREAMS.clear();

        let (tx, _rx) = unbounded::<ControlPacket>();
        let client = ConnectedClient::new(ClientId::new(), "host-aaa".to_string(), tx);
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
