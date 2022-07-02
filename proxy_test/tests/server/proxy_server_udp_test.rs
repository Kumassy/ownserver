// integrated server test
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use magic_tunnel_client::proxy_client::{send_client_hello, verify_server_hello, ClientInfo};
use magic_tunnel_lib::{ClientId, ControlPacket};
use magic_tunnel_server::{
    active_stream::ActiveStreams, connected_clients::Connections, port_allocator::PortAllocator,
    proxy_server::run,
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
use tokio_util::sync::CancellationToken;

#[cfg(test)]
mod server_udp_test {
    use super::*;
    use magic_tunnel_lib::Payload;
    use serial_test::serial;
    use tokio::net::UdpSocket;
    use tracing::info;

    static CONFIG: OnceCell<Config> = OnceCell::new();

    macro_rules! assert_control_packet_type_matches {
        ($expr:expr, $pat:pat) => {
            let payload = $expr.next().await.unwrap()?.into_data();
            let control_packet: ControlPacket = rmp_serde::from_slice(&payload)?;
            assert!(matches!(control_packet, $pat));
        };
    }

    macro_rules! assert_control_packet_matches {
        ($expr:expr, $expected:expr) => {
            let payload = $expr.next().await.unwrap()?.into_data();
            let control_packet: ControlPacket = rmp_serde::from_slice(&payload)?;
            assert_eq!(control_packet, $expected);
        };
    }

    macro_rules! assert_socket_bytes_matches {
        ($read:expr, $expected:expr) => {
            let mut buf = [0; 4 * 1024];
            let n = $read
                .recv(&mut buf)
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
            pub static ref ACTIVE_STREAMS: ActiveStreams = ActiveStreams::default();
        }

        let config = CONFIG.get_or_init(||
            Config {
                control_port: 5000,
                token_secret: "supersecret".to_string(),
                host: "127.0.0.1".to_string(),
                remote_port_start: 4000,
                remote_port_end: 4010,
            }
        );

        // we must clear CONNECTIONS, ACTIVE_STREAMS
        // because they are shared across test
        Connections::clear(&CONNECTIONS);
        ACTIVE_STREAMS.clear();
        let alloc = Arc::new(Mutex::new(PortAllocator::new(config.remote_port_start..config.remote_port_end)));
        let remote_cancellers: Arc<DashMap<ClientId, CancellationToken>> = Arc::new(DashMap::new());

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
        let token = make_jwt("supersecret", CDuration::minutes(10), "127.0.0.1".to_string())?;

        // --- handshake
        let url = Url::parse(&format!("wss://localhost:{}/tunnel", control_port))?;
        let (mut websocket, _) = connect_async(url).await.expect("failed to connect");

        send_client_hello(&mut websocket, token, Payload::UDP).await?;
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

        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote.connect(client_info.remote_addr).await.unwrap();
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        remote
            .send(b"some bytes")
            .await
            .expect("failed to send client hello");
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams.iter().count(),
            1,
            "remote socket should be accepted and registered"
        );
        let stream_id = active_streams.iter().next().unwrap().id;

        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::UdpData(stream_id, b"some bytes".to_vec())
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
        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            remote.connect(client_info.remote_addr).await.unwrap();
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        // send something so that remote connection registered to active_streams
        remote
            .send(b"some bytes")
            .await
            .expect("failed to send client hello");
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams.iter().count(),
            1,
            "remote socket should be accepted and registered"
        );
        let stream_id = active_streams.iter().next().unwrap().id;
        raw_client_ws_sink
            .send(Message::binary(
                rmp_serde::to_vec(&ControlPacket::UdpData(stream_id, b"foobarbaz".to_vec())).unwrap()
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
        let remote1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            remote1.connect(client_info.remote_addr.clone()).await.unwrap();
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        let remote2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote2.connect(client_info.remote_addr.clone()).await.unwrap();
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        remote1
            .send(b"some bytes 1")
            .await
            .expect("failed to send client hello");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            active_streams.iter().count(),
            1,
            "remote socket should be registered once remote send any data"
        );

        remote2
            .send(b"some bytes 2")
            .await
            .expect("failed to send client hello");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            active_streams.iter().count(),
            2,
            "remote socket should be accepted and registered"
        );

        let stream_id1 = active_streams.find_by_addr(&remote1.local_addr().expect("failed to get local_addr")).expect("unable to find active_stream by addr").id;
        let stream_id2 = active_streams.find_by_addr(&remote2.local_addr().expect("failed to get local_addr")).expect("unable to find active_stream by addr").id;

        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::UdpData(stream_id1, b"some bytes 1".to_vec())
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::UdpData(stream_id2, b"some bytes 2".to_vec())
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
        let remote1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            remote1.connect(client_info.remote_addr.clone()).await.unwrap();
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        let remote2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote2.connect(client_info.remote_addr.clone()).await.unwrap();
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        remote1
            .send(b"some bytes 1")
            .await
            .expect("failed to send client hello");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            active_streams.iter().count(),
            1,
            "remote socket should be registered once remote send any data"
        );

        remote2
            .send(b"some bytes 2")
            .await
            .expect("failed to send client hello");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            active_streams.iter().count(),
            2,
            "remote socket should be accepted and registered"
        );

        let stream_id1 = active_streams.find_by_addr(&remote1.local_addr().expect("failed to get local_addr")).expect("unable to find active_stream by addr").id;
        let stream_id2 = active_streams.find_by_addr(&remote2.local_addr().expect("failed to get local_addr")).expect("unable to find active_stream by addr").id;

        raw_client_ws_sink
            .send(Message::binary(
                rmp_serde::to_vec(&ControlPacket::UdpData(stream_id1, b"some message 1".to_vec())).unwrap()
            ))
            .await?;
        raw_client_ws_sink
            .send(Message::binary(
                rmp_serde::to_vec(&ControlPacket::UdpData(stream_id2, b"some message 2".to_vec())).unwrap()
            ))
            .await?;

        assert_socket_bytes_matches!(remote1, b"some message 1");
        assert_socket_bytes_matches!(remote2, b"some message 2");
        Ok(())
    }
}