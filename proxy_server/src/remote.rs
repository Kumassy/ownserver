use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tracing::debug;
use tracing::{error, Instrument};
use futures::prelude::*;

use crate::active_stream::{ActiveStream, StreamMessage};
use crate::connected_clients::{ConnectedClient, Connections};
use crate::control_server;
pub use magic_tunnel_lib::{StreamId, ClientId, ControlPacket};

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
            debug!("stream ended");
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

        let data = &buf[..n];
        let packet = ControlPacket::Data(tunnel_stream.id.clone(), data.to_vec());

        match tunnel_stream.client.tx.send(packet.clone()).await {
            Ok(_) => debug!(client_id = %tunnel_stream.client.id, "sent data packet to client"),
            Err(_) => {
                // TODO: not tested
                error!("failed to forward tcp packets to disconnected client. dropping client.");
                Connections::remove(conn, &tunnel_stream.client);
            }
        }
    }
}

#[cfg(test)]
mod process_tcp_stream_test {
    use super::*;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use tokio_test::io::Builder;

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
        let stream_id = active_stream.id.clone();

        let tcp_mock = Builder::new()
            .build();
        let _ = process_tcp_stream(&conn, active_stream, tcp_mock).await;

        assert_eq!(client_rx.next().await, None);
        Ok(())
    }
}
