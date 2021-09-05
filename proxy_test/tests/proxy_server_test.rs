// integrated server test
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use magic_tunnel_client::proxy_client::{send_client_hello, verify_server_hello, ClientInfo};
use magic_tunnel_lib::{ClientId, ControlPacket};
use magic_tunnel_server::{
    active_stream::ActiveStreams, connected_clients::Connections, port_allocator::PortAllocator,
    proxy_server::run, remote::CancelHander,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use url::Url;
use once_cell::sync::OnceCell;
use magic_tunnel_server::Config;
use magic_tunnel_auth::make_jwt;
use chrono::Duration as CDuration;

#[cfg(test)]
mod proxy_server_test {
    use super::*;
    use serial_test::serial;

    static CONFIG: OnceCell<Config> = OnceCell::new();

    macro_rules! assert_control_packet_type_matches {
        ($expr:expr, $pat:pat) => {
            let payload = $expr.next().await.unwrap()?.into_data();
            let control_packet = ControlPacket::deserialize(&payload)?;
            assert!(matches!(control_packet, $pat));
        };
    }

    macro_rules! assert_control_packet_matches {
        ($expr:expr, $expected:expr) => {
            let payload = $expr.next().await.unwrap()?.into_data();
            let control_packet = ControlPacket::deserialize(&payload)?;
            assert_eq!(control_packet, $expected);
        };
    }

    macro_rules! assert_socket_bytes_matches {
        ($read:expr, $expected:expr) => {
            let mut buf = [0; 4 * 1024];
            let n = $read
                .read(&mut buf)
                .await
                .expect("failed to read data from socket");
            let data = buf[..n].to_vec();

            assert_eq!(data, $expected);
        };
    }

    async fn launch_proxy_server(
        control_port: u16,
    ) -> Result<
        (
            WebSocketStream<MaybeTlsStream<TcpStream>>,
            ActiveStreams,
            ClientInfo,
        ),
        Box<dyn std::error::Error>,
    > {
        lazy_static! {
            pub static ref CONNECTIONS: Connections = Connections::new();
            pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(DashMap::new());
        }

        let config = CONFIG.get_or_init(||
            Config {
                control_port: 5000,
                token_secret: "supersecret".to_string(),
                host: "integrated.test.local".to_string(),
                remote_port_start: 4000,
                remote_port_end: 4010,
            }
        );

        // we must clear CONNECTIONS, ACTIVE_STREAMS
        // because they are shared across test
        Connections::clear(&CONNECTIONS);
        ACTIVE_STREAMS.clear();
        let alloc = Arc::new(Mutex::new(PortAllocator::new(config.remote_port_start..config.remote_port_end)));
        let remote_cancellers: Arc<DashMap<ClientId, CancelHander>> = Arc::new(DashMap::new());

        tokio::spawn(async move {
            run(
                &CONFIG,
                &CONNECTIONS,
                &ACTIVE_STREAMS,
                alloc,
                remote_cancellers,
            )
            .await.unwrap();
        });

        // setup proxy client
        // --- generate valid jwt
        let token = make_jwt("supersecret", CDuration::minutes(10), "integrated.test.local".to_string())?;

        // --- handshake
        let url = Url::parse(&format!("wss://localhost:{}/tunnel", control_port))?;
        let (mut websocket, _) = connect_async(url).await.expect("failed to connect");

        send_client_hello(&mut websocket, token).await?;
        let client_info = verify_server_hello(&mut websocket).await?;

        Ok((websocket, ACTIVE_STREAMS.clone(), client_info))
    }

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_client() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let (websoket, active_streams, client_info) = launch_proxy_server(control_port).await?;
        let (mut _raw_client_ws_sink, mut raw_client_ws_stream) = websoket.split();

        assert_eq!(
            active_streams.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        let mut remote = TcpStream::connect(format!("127.0.0.1:{}", client_info.assigned_port))
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;
        remote
            .write_all(b"some bytes")
            .await
            .expect("failed to send client hello");

        assert_eq!(
            active_streams.iter().count(),
            1,
            "remote socket should be accepted and registered"
        );
        let stream_id = active_streams.iter().next().unwrap().id.clone();

        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Init(stream_id.clone())
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Data(stream_id.clone(), b"some bytes".to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_client_traffic_to_remote() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let (websoket, active_streams, client_info) = launch_proxy_server(control_port).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        assert_eq!(
            active_streams.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        let mut remote = TcpStream::connect(format!("127.0.0.1:{}", client_info.assigned_port))
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams.iter().count(),
            1,
            "remote socket should be accepted and registered"
        );
        let stream_id = active_streams.iter().next().unwrap().id.clone();
        raw_client_ws_sink
            .send(Message::binary(
                ControlPacket::Data(stream_id, b"foobarbaz".to_vec()).serialize(),
            ))
            .await?;

        assert_socket_bytes_matches!(remote, b"foobarbaz");
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_client() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let (websoket, active_streams, client_info) = launch_proxy_server(control_port).await?;
        let (mut _raw_client_ws_sink, mut raw_client_ws_stream) = websoket.split();

        assert_eq!(
            active_streams.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        let mut remote1 = TcpStream::connect(format!("127.0.0.1:{}", client_info.assigned_port))
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            active_streams.iter().count(),
            1,
            "remote socket should be accepted and registered"
        );
        let stream_id1 = active_streams.iter().next().unwrap().id.clone();

        let mut remote2 = TcpStream::connect(format!("127.0.0.1:{}", client_info.assigned_port))
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            active_streams.iter().count(),
            2,
            "remote socket should be accepted and registered"
        );
        let stream_id2 = active_streams
            .iter()
            .filter(|sid| sid.key() != &stream_id1)
            .next()
            .unwrap()
            .id
            .clone();

        assert_ne!(stream_id1, stream_id2);

        remote1
            .write_all(b"some bytes 1")
            .await
            .expect("failed to send client hello");
        tokio::time::sleep(Duration::from_secs(3)).await;
        remote2
            .write_all(b"some bytes 2")
            .await
            .expect("failed to send client hello");

        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Init(stream_id1.clone())
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Init(stream_id2.clone())
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Data(stream_id1.clone(), b"some bytes 1".to_vec())
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Data(stream_id2.clone(), b"some bytes 2".to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_client_traffic_to_multiple_remote() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let (websoket, active_streams, client_info) = launch_proxy_server(control_port).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        assert_eq!(
            active_streams.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        let mut remote1 = TcpStream::connect(format!("127.0.0.1:{}", client_info.assigned_port))
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            active_streams.iter().count(),
            1,
            "remote socket should be accepted and registered"
        );
        let stream_id1 = active_streams.iter().next().unwrap().id.clone();

        let mut remote2 = TcpStream::connect(format!("127.0.0.1:{}", client_info.assigned_port))
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            active_streams.iter().count(),
            2,
            "remote socket should be accepted and registered"
        );
        let stream_id2 = active_streams
            .iter()
            .filter(|sid| sid.key() != &stream_id1)
            .next()
            .unwrap()
            .id
            .clone();

        assert_ne!(stream_id1, stream_id2);

        raw_client_ws_sink
            .send(Message::binary(
                ControlPacket::Data(stream_id1, b"some message 1".to_vec()).serialize(),
            ))
            .await?;
        raw_client_ws_sink
            .send(Message::binary(
                ControlPacket::Data(stream_id2, b"some message 2".to_vec()).serialize(),
            ))
            .await?;

        assert_socket_bytes_matches!(remote1, b"some message 1");
        assert_socket_bytes_matches!(remote2, b"some message 2");
        Ok(())
    }
}
