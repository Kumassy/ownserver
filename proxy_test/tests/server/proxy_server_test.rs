use magic_tunnel_lib::Payload;
use magic_tunnel_server::{proxy_server::run, Store};
use serial_test::serial;
use futures::{SinkExt, StreamExt};
use magic_tunnel_client::proxy_client::{send_client_hello, verify_server_hello, ClientInfo};
use magic_tunnel_lib::ControlPacket;
use magic_tunnel_server::{
    port_allocator::PortAllocator,
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
use tokio::net::UdpSocket;

#[cfg(test)]
mod server_tcp_test {
    use super::*;
    static CONFIG: OnceCell<Config> = OnceCell::new();

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
                .read(&mut buf)
                .await
                .expect("failed to read data from socket");
            let data = buf[..n].to_vec();

            assert_eq!(data, $expected);
        };
    }

    macro_rules! wait {
        () => {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    async fn launch_proxy_server(
        control_port: u16,
    ) -> Result<
        (
            WebSocketStream<MaybeTlsStream<TcpStream>>,
            Arc<Store>,
            ClientInfo,
        ),
        Box<dyn std::error::Error>,
    > {

        let config = CONFIG.get_or_init(||
            Config {
                control_port: 5000,
                token_secret: "supersecret".to_string(),
                host: "127.0.0.1".to_string(),
                remote_port_start: 4000,
                remote_port_end: 4010,
            }
        );

        let alloc = Arc::new(Mutex::new(PortAllocator::new(config.remote_port_start..config.remote_port_end)));
        let store = Arc::new(Store::default());

        let store_ = store.clone();
        tokio::spawn(async move {
            run(
                &CONFIG,
                store_,
                alloc,
            )
            .await.unwrap();
        });

        // setup proxy client
        // --- generate valid jwt
        let token = make_jwt("supersecret", CDuration::minutes(10), "127.0.0.1".to_string())?;

        // --- handshake
        let url = Url::parse(&format!("wss://localhost:{}/tunnel", control_port))?;
        let (mut websocket, _) = connect_async(url).await.expect("failed to connect");

        send_client_hello(&mut websocket, token, Payload::Other).await?;
        let client_info = verify_server_hello(&mut websocket).await?;

        Ok((websocket, store, client_info))
    }


    const CONTROL_PORT: u16 = 5000;

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_client() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut _raw_client_ws_sink, mut raw_client_ws_stream) = websoket.split();

        let mut remote = TcpStream::connect(client_info.remote_addr)
            .await
            .expect("Failed to connect to remote port");
        remote
            .write_all(b"some bytes")
            .await
            .expect("failed to send client hello");

        wait!();
        let stream_id = store.streams.iter().next().unwrap().stream_id();

        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Init(stream_id)
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Data(stream_id, b"some bytes".to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_client_traffic_to_remote() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        let mut remote = TcpStream::connect(client_info.remote_addr)
            .await
            .expect("Failed to connect to remote port");

        wait!();
        let stream_id = store.streams.iter().next().unwrap().stream_id();

        raw_client_ws_sink
            .send(Message::binary(
                rmp_serde::to_vec(&ControlPacket::Data(stream_id, b"foobarbaz".to_vec())).unwrap()
            ))
            .await?;

        assert_socket_bytes_matches!(remote, b"foobarbaz");
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_client() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut _raw_client_ws_sink, mut raw_client_ws_stream) = websoket.split();

        let mut remote1 = TcpStream::connect(client_info.remote_addr.clone())
            .await
            .expect("Failed to connect to remote port");
        wait!();
        let stream_id1 = store.streams.iter().next().unwrap().stream_id();

        let mut remote2 = TcpStream::connect(client_info.remote_addr.clone())
            .await
            .expect("Failed to connect to remote port");
        wait!();
        let stream_id2 = store.streams
            .iter()
            .find(|sid| sid.key() != &stream_id1)
            .unwrap()
            .stream_id();

        assert_ne!(stream_id1, stream_id2);

        remote1
            .write_all(b"some bytes 1")
            .await
            .expect("failed to send client hello");
        remote2
            .write_all(b"some bytes 2")
            .await
            .expect("failed to send client hello");
        wait!();

        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Init(stream_id1)
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Init(stream_id2)
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Data(stream_id1, b"some bytes 1".to_vec())
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::Data(stream_id2, b"some bytes 2".to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_client_traffic_to_multiple_remote() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        let mut remote1 = TcpStream::connect(client_info.remote_addr.clone())
            .await
            .expect("Failed to connect to remote port");
        wait!();
        let stream_id1 = store.streams.iter().next().unwrap().stream_id();

        let mut remote2 = TcpStream::connect(client_info.remote_addr.clone())
            .await
            .expect("Failed to connect to remote port");
        wait!();
        let stream_id2 = store.streams
            .iter()
            .find(|sid| sid.key() != &stream_id1)
            .unwrap()
            .stream_id();

        assert_ne!(stream_id1, stream_id2);

        raw_client_ws_sink
            .send(Message::binary(
                rmp_serde::to_vec(&ControlPacket::Data(stream_id1, b"some message 1".to_vec())).unwrap()
            ))
            .await?;
        raw_client_ws_sink
            .send(Message::binary(
                rmp_serde::to_vec(&ControlPacket::Data(stream_id2, b"some message 2".to_vec())).unwrap()
            ))
            .await?;

        assert_socket_bytes_matches!(remote1, b"some message 1");
        assert_socket_bytes_matches!(remote2, b"some message 2");
        Ok(())
    }
}



#[cfg(test)]
mod server_udp_test {

    use super::*;
    static CONFIG: OnceCell<Config> = OnceCell::new();

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

    macro_rules! wait {
        () => {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    async fn launch_proxy_server(
        control_port: u16,
    ) -> Result<
        (
            WebSocketStream<MaybeTlsStream<TcpStream>>,
            Arc<Store>,
            ClientInfo,
        ),
        Box<dyn std::error::Error>,
    > {
        let config = CONFIG.get_or_init(||
            Config {
                control_port: 5000,
                token_secret: "supersecret".to_string(),
                host: "127.0.0.1".to_string(),
                remote_port_start: 4100,
                remote_port_end: 4110,
            }
        );
        let alloc = Arc::new(Mutex::new(PortAllocator::new(config.remote_port_start..config.remote_port_end)));
        let store = Arc::new(Store::default());

        let store_ = store.clone();
        tokio::spawn(async move {
            run(
                &CONFIG,
                store_,
                alloc,
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

        Ok((websocket, store, client_info))
    }

    const CONTROL_PORT: u16 = 5000;

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_client() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut _raw_client_ws_sink, mut raw_client_ws_stream) = websoket.split();

        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote.connect(client_info.remote_addr).await.unwrap();
        wait!();

        remote
            .send(b"some bytes")
            .await
            .expect("failed to send client hello");

        wait!();
        let stream_id = store.streams.iter().next().unwrap().stream_id();

        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacket::UdpData(stream_id, b"some bytes".to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_client_traffic_to_remote() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            remote.connect(client_info.remote_addr).await.unwrap();

        // send something for remote stream to be registerd to store
        remote
            .send(b"some bytes")
            .await
            .expect("failed to send client hello");

        wait!();
        let stream_id = store.streams.iter().next().unwrap().stream_id();

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
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut _raw_client_ws_sink, mut raw_client_ws_stream) = websoket.split();

        let remote1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote1.connect(client_info.remote_addr.clone()).await.unwrap();
        // send something for remote stream to be registerd to store
        remote1
            .send(b"some bytes 1")
            .await
            .expect("failed to send client hello");
        wait!();
        let stream_id1 = store.streams.iter().next().unwrap().stream_id();

        let remote2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote2.connect(client_info.remote_addr.clone()).await.unwrap();
        // send something for remote stream to be registerd to store
        remote2
            .send(b"some bytes 2")
            .await
            .expect("failed to send client hello");
        wait!();
        let stream_id2 = store.streams
            .iter()
            .find(|sid| sid.key() != &stream_id1)
            .unwrap()
            .stream_id();


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
        pretty_env_logger::init();
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        let remote1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote1.connect(client_info.remote_addr.clone()).await.unwrap();
        // send something for remote stream to be registerd to store
        remote1
            .send(b"some bytes 1")
            .await
            .expect("failed to send client hello");
        wait!();
        let stream_id1 = store.streams.iter().next().unwrap().stream_id();

        let remote2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote2.connect(client_info.remote_addr.clone()).await.unwrap();
        // send something for remote stream to be registerd to store
        remote2
            .send(b"some bytes 2")
            .await
            .expect("failed to send client hello");
        wait!();
        let stream_id2 = store.streams
            .iter()
            .find(|sid| sid.key() != &stream_id1)
            .unwrap()
            .stream_id();

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