use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tracing::debug;
use tracing::{error, Instrument};
use futures::prelude::*;
use futures::channel::mpsc::UnboundedReceiver;

use crate::active_stream::{ActiveStream, StreamMessage, ActiveStreams};
use crate::connected_clients::{ConnectedClient, Connections};
use crate::control_server;
pub use magic_tunnel_lib::{StreamId, ClientId, ControlPacket};

// stream has selected because each stream listen different port
#[tracing::instrument(skip(socket, conn, active_streams))]
pub async fn accept_connection(conn: &'static Connections, active_streams: ActiveStreams, mut socket: TcpStream, host: String) {
    tracing::info!("new remote connection");

    // find the client listening for this host
    let client = match Connections::find_by_host(conn, &host) {
        Some(client) => client.clone(),
        None => {
            error!(%host, "failed to find instance");
            let _ = socket.write_all(HTTP_ERROR_LOCATING_HOST_RESPONSE).await;
            return;
        }
    };

    // allocate a new stream for this request
    let (active_stream, queue_rx) = ActiveStream::new(client.clone());
    let stream_id = active_stream.id.clone();

    tracing::debug!(
        stream_id = %active_stream.id.to_string(),
        "new stream connected"
    );
    let (stream, sink) = tokio::io::split(socket);

    // add our stream
    active_streams.insert(stream_id.clone(), active_stream.clone());

    // read from socket, write to client
    tokio::spawn(
        async move {
            process_tcp_stream(conn, active_stream, stream).await;
        }
        .instrument(tracing::info_span!("process_tcp_stream")),
    );

    // read from client, write to socket
    tokio::spawn(
        async move {
            let reason = tunnel_to_stream(host, stream_id.clone(), sink, queue_rx).await;
            debug!("tunnel_to_stream closed for stream_id: {:?} with reason: {:?}", stream_id, reason);
            active_streams.remove(&stream_id);
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
#[tracing::instrument(skip(tunnel_stream, tcp_stream))]
// async fn process_tcp_stream(conn: &Connections, mut tunnel_stream: ActiveStream, mut tcp_stream: ReadHalf<TcpStream>) {
async fn process_tcp_stream<T>(conn: &Connections, mut tunnel_stream: ActiveStream, mut tcp_stream: T)
where T: AsyncRead + Unpin {
    // send initial control stream init to client
    if let Err(e) = control_server::send_client_stream_init(tunnel_stream.clone()).await {
        error!("failed to send stream init: {:?}", e);
        return;
    }

    // now read from stream and forward to clients
    let mut buf = [0; 1024];

    loop {
        // client is no longer connected
        if Connections::get(conn, &tunnel_stream.client.id).is_none() {
            debug!("client disconnected, closing stream");

            // close remote-writer channel
            let _ = tunnel_stream.tx.send(StreamMessage::NoClientTunnel).await;
            tunnel_stream.tx.close_channel();
            return;
        }

        // read from stream
        let n = match tcp_stream.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                error!("failed to read from tcp socket: {:?}", e);
                return;
            }
        };

        if n == 0 {
            debug!("stream {:?} ended", tunnel_stream.id);
            let _ = tunnel_stream
                .client
                .tx
                .send(ControlPacket::End(tunnel_stream.id.clone()))
                .await
                .map_err(|e| {
                    error!("failed to send end signal: {:?}", e);
                });
            return;
        }

        debug!("read {} bytes", n);

        if tunnel_stream.tx.is_closed() {
            tracing::debug!("process_tcp_stream closed because active_stream.tx has closed");
            return;
        }

        let data = &buf[..n];
        let packet = ControlPacket::Data(tunnel_stream.id.clone(), data.to_vec());

        match tunnel_stream.client.tx.send(packet.clone()).await {
            Ok(_) => debug!(client_id = %tunnel_stream.client.id, "sent data packet to client"),
            Err(_) => {
                // TODO: not tested
                // This line extecuted when
                // - Corresponding client not found or closed
                error!("failed to forward tcp packets to disconnected client. dropping client.");
                Connections::remove(conn, &tunnel_stream.client);
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
)
-> TunnelToStreamExitReason
where T: AsyncWrite + AsyncWriteExt + Unpin
{
    loop {
        let result = queue.next().await;

        let result = if let Some(message) = result {
            match message {
                StreamMessage::Data(data) => Some(data),
                StreamMessage::TunnelRefused => {
                    tracing::debug!(?stream_id, "tunnel refused");
                    let _ = sink.write_all(HTTP_TUNNEL_REFUSED_RESPONSE).await;
                    None
                }
                StreamMessage::NoClientTunnel => {
                    tracing::info!(%subdomain, ?stream_id, "client tunnel not found");
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
                tracing::debug!("done tunneling to sink");
                let _ = sink.shutdown().await.map_err(|_e| {
                    error!("error shutting down tcp stream");
                });

                // active_streams.remove(&stream_id);
                // return;
                return TunnelToStreamExitReason::QueueClosed;
            }
        };

        let result = sink.write_all(&data).await;

        if let Some(error) = result.err() {
            tracing::warn!(?error, "stream closed, disconnecting");
            return TunnelToStreamExitReason::TcpClosed;
        }
    }
}

#[cfg(test)]
mod process_tcp_stream_test {
    use super::*;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use tokio_test::io::Builder;

    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
    use std::task::{self, Poll, Waker};
    use std::pin::Pin;
    use std::io;

    struct InfiniteRead {
    }
    impl InfiniteRead {
        fn new() -> Self {
            InfiniteRead {}
        }
    }
    impl AsyncRead for InfiniteRead {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            buf.put_slice(b"infinite source");
            return Poll::Ready(Ok(()));
        }
    }

    fn create_active_stream() -> (Connections, ConnectedClient, ActiveStream, UnboundedReceiver<StreamMessage>, UnboundedReceiver<ControlPacket>) {
        let conn = Connections::new();
        let (tx, rx) = unbounded::<ControlPacket>();
        let client = ConnectedClient {
            id: ClientId::generate(),
            host: "foobar".into(),
            tx
        };

        let (active_stream, stream_rx) = ActiveStream::new(client.clone());

        (conn, client, active_stream, stream_rx, rx)
    }

    #[tokio::test]
    async fn send_stream_init() -> Result<(), Box<dyn std::error::Error>> {
        let (conn, _client, active_stream, _stream_rx, mut client_rx) = create_active_stream();

        let tcp_mock = Builder::new()
            .build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert!(matches!(client_rx.next().await.unwrap(), ControlPacket::Init(_)));
        Ok(())
    }
    
    #[tokio::test]
    async fn send_noclienttunnel_to_remote_when_client_not_registered() -> Result<(), Box<dyn std::error::Error>> {
        let (conn, _client, active_stream, mut stream_rx, _client_rx) = create_active_stream();

        let tcp_mock = Builder::new()
            .build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(stream_rx.next().await.unwrap(), StreamMessage::NoClientTunnel);
        Ok(())
    }

    #[tokio::test]
    async fn send_stream_end() -> Result<(), Box<dyn std::error::Error>> {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        Connections::add(&conn, client.clone());
        let stream_id = active_stream.id.clone();

        let tcp_mock = Builder::new()
            .build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(client_rx.next().await.unwrap(), ControlPacket::Init(stream_id.clone()));
        assert_eq!(client_rx.next().await.unwrap(), ControlPacket::End(stream_id.clone()));

        Ok(())
    }

    #[tokio::test]
    async fn client_stream_should_close_when_client_is_dropped() -> Result<(), Box<dyn std::error::Error>> {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        Connections::add(&conn, client.clone());

        let tcp_mock = Builder::new()
            .build();
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

        let tcp_mock = Builder::new()
            .read(b"foobar")
            .build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(client_rx.next().await.unwrap(), ControlPacket::Init(stream_id.clone()));
        assert_eq!(client_rx.next().await.unwrap(), ControlPacket::Data(stream_id.clone(), b"foobar".to_vec()));
        assert_eq!(client_rx.next().await.unwrap(), ControlPacket::End(stream_id.clone()));
        Ok(())
    }

    #[tokio::test]
    async fn failed_to_send_stream_init_when_client_closed() -> Result<(), Box<dyn std::error::Error>> {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        client.tx.close_channel();
        Connections::add(&conn, client);

        let tcp_mock = Builder::new()
            .build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(client_rx.next().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn stop_wait_read_when_tunnel_stream_is_closed() -> Result<(), Box<dyn std::error::Error>> {
        let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
        Connections::add(&conn, client);
        let stream_id = active_stream.id.clone();

        client_rx.close();
        let infinite_reader = InfiniteRead::new();
        let _ = process_tcp_stream(&conn, active_stream, infinite_reader).await;

        Ok(())
    }
}

#[cfg(test)]
mod tunnel_to_stream_test {
    use super::*;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use tokio_test::io::Builder;
    use std::io;

    #[tokio::test]
    async fn mock_test() -> Result<(), Box<dyn std::error::Error>> {
        let mut tcp_mock = Builder::new()
            .write(b"foobarbaz")
            .write(b"piyo")
            .build();

        // writed data must be exactly same as builder args
        // any grouping is accepted because of stream
        tcp_mock.write(b"foobar").await?;
        tcp_mock.write(b"bazpiyo").await?;
        Ok(())
    }
    
    #[tokio::test]
    async fn must_exit_from_loop_when_tcp_raises_error() -> Result<(), Box<dyn std::error::Error>> {
        let error = io::Error::new(io::ErrorKind::Other, "cruel");
        let tcp_mock = Builder::new()
            .write_error(error)
            .build();
        let (mut tx, rx) = unbounded();

        tx.send(StreamMessage::Data(b"foobar".to_vec())).await?;
        let reason = tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;

        assert_eq!(reason, TunnelToStreamExitReason::TcpClosed);
        assert_eq!(tx.is_closed(), true);
        Ok(())
    }

    #[tokio::test]
    async fn tcp_stream_must_shutdown_when_queue_is_closed() -> Result<(), Box<dyn std::error::Error>> {
        let tcp_mock = Builder::new()
            .build();
        let (tx, rx) = unbounded();
        tx.close_channel();

        let reason = tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;
        assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        Ok(())
    }

    #[tokio::test]
    async fn tcp_stream_must_shutdown_when_tunnel_refused() -> Result<(), Box<dyn std::error::Error>> {
        let tcp_mock = Builder::new()
            .build();
        let (mut tx, rx) = unbounded();

        tx.send(StreamMessage::TunnelRefused).await?;
        let reason = tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;

        assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        assert_eq!(tx.is_closed(), true);
        Ok(())
    }

    #[tokio::test]
    async fn tcp_stream_must_shutdown_when_no_client_tunnel() -> Result<(), Box<dyn std::error::Error>> {
        let tcp_mock = Builder::new()
            .build();
        let (mut tx, rx) = unbounded();

        tx.send(StreamMessage::NoClientTunnel).await?;
        let reason = tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;

        assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        assert_eq!(tx.is_closed(), true);
        Ok(())
    }

    #[tokio::test]
    async fn forward_data() -> Result<(), Box<dyn std::error::Error>> {
        let tcp_mock = Builder::new()
            .write(b"foobarbaz")
            .build();
        let (mut tx, rx) = unbounded();

        tx.send(StreamMessage::Data(b"foobarbaz".to_vec())).await?;
        tx.close_channel();
        let reason = tunnel_to_stream("foobar".to_string(), StreamId::generate(), tcp_mock, rx).await;

        assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        Ok(())
    }
}