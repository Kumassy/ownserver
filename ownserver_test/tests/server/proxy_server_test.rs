use ownserver_server::{proxy_server::run, Store};
use serial_test::serial;
use futures::{SinkExt, StreamExt};
use ownserver::proxy_client::{send_client_hello, verify_server_hello, ClientInfo};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use url::Url;
use once_cell::sync::OnceCell;
use ownserver_server::Config;
use ownserver_auth::make_jwt;
use chrono::Duration as CDuration;
use tokio::net::UdpSocket;
use tokio_util::{codec::{Encoder, Decoder}};
use bytes::BytesMut;

#[cfg(test)]
mod server_tcp_test {
    use ownserver_lib::{EndpointClaim, Protocol, ControlPacketV2, ControlPacketV2Codec};

    use super::*;
    static CONFIG: OnceCell<Config> = OnceCell::new();

    macro_rules! assert_control_packet_matches {
        ($expr:expr, $expected:expr) => {
            let payload = $expr.next().await.unwrap()?.into_data();
            let mut bytes = BytesMut::from(&payload[..]);
            let control_packet = ControlPacketV2Codec::new().decode(&mut bytes)?.unwrap();
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
                remote_port_end: 4099,
                periodic_cleanup_interval: 2 << 30,
                periodic_ping_interval: 2 << 30,
            }
        );

        let store = Arc::new(Store::new(config.remote_port_start..config.remote_port_end));

        let store_ = store.clone();
        tokio::spawn(async move {
            run(
                &CONFIG,
                store_,
            )
            .await.join_next().await;
        });

        // setup proxy client
        // --- generate valid jwt
        let token = make_jwt("supersecret", CDuration::minutes(10), "127.0.0.1".to_string())?;

        // --- handshake
        let url = Url::parse(&format!("ws://localhost:{}/tunnel", control_port))?;
        let (mut websocket, _) = connect_async(url).await.expect("failed to connect");

        let endpoint_claims = vec![EndpointClaim {
            protocol: Protocol::TCP,
            local_port: 0,
            remote_port: 0,
        }];
        send_client_hello(&mut websocket, token, endpoint_claims).await?;
        let client_info = verify_server_hello(&mut websocket).await?;

        Ok((websocket, store, client_info))
    }


    const CONTROL_PORT: u16 = 5000;

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_client() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut _raw_client_ws_sink, mut raw_client_ws_stream) = websoket.split();

        let mut remote = TcpStream::connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port))
            .await
            .expect("Failed to connect to remote port");
        remote
            .write_all(b"some bytes")
            .await
            .expect("failed to send client hello");

        wait!();
        let stream_id = store.get_stream_ids().await[0];

        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Init(stream_id, client_info.endpoints[0].id)
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Data(stream_id, b"some bytes".to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_client_traffic_to_remote() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        let mut remote = TcpStream::connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port))
            .await
            .expect("Failed to connect to remote port");

        wait!();
        let stream_id = store.get_stream_ids().await[0];

        let mut codec = ControlPacketV2Codec::new();
        let mut bytes = BytesMut::new();
        codec.encode(ControlPacketV2::Data(stream_id, b"foobarbaz".to_vec()), &mut bytes)?;

        raw_client_ws_sink
            .send(Message::binary(bytes.to_vec()))
            .await?;

        assert_socket_bytes_matches!(remote, b"foobarbaz");
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_client() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut _raw_client_ws_sink, mut raw_client_ws_stream) = websoket.split();

        let mut remote1 = TcpStream::connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port))
            .await
            .expect("Failed to connect to remote port");
        wait!();
        let stream_id1 = store.get_stream_ids().await[0];

        let mut remote2 = TcpStream::connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port))
            .await
            .expect("Failed to connect to remote port");
        wait!();
        let stream_id2 = store.get_stream_ids().await
            .into_iter()
            .find(|sid| sid != &stream_id1)
            .unwrap();

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
            ControlPacketV2::Init(stream_id1, client_info.endpoints[0].id)
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Init(stream_id2, client_info.endpoints[0].id)
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Data(stream_id1, b"some bytes 1".to_vec())
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Data(stream_id2, b"some bytes 2".to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_client_traffic_to_multiple_remote() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        let mut remote1 = TcpStream::connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port))
            .await
            .expect("Failed to connect to remote port");
        wait!();
        let stream_id1 = store.get_stream_ids().await[0];

        let mut remote2 = TcpStream::connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port))
            .await
            .expect("Failed to connect to remote port");
        wait!();
        let stream_id2 = store.get_stream_ids().await
            .into_iter()
            .find(|sid| sid != &stream_id1)
            .unwrap();

        assert_ne!(stream_id1, stream_id2);

        let mut codec = ControlPacketV2Codec::new();
        let mut bytes = BytesMut::new();
        codec.encode(ControlPacketV2::Data(stream_id1, b"some message 1".to_vec()), &mut bytes)?;
        raw_client_ws_sink
            .send(Message::binary(bytes.to_vec()))
            .await?;

        let mut codec = ControlPacketV2Codec::new();
        let mut bytes = BytesMut::new();
        codec.encode(ControlPacketV2::Data(stream_id2, b"some message 2".to_vec()), &mut bytes)?;
        raw_client_ws_sink
            .send(Message::binary(bytes.to_vec()))
            .await?;

        assert_socket_bytes_matches!(remote1, b"some message 1");
        assert_socket_bytes_matches!(remote2, b"some message 2");
        Ok(())
    }
}



#[cfg(test)]
mod server_udp_test {

    use ownserver_lib::{EndpointClaim, Protocol, ControlPacketV2Codec, ControlPacketV2};

    use super::*;
    static CONFIG: OnceCell<Config> = OnceCell::new();

    macro_rules! assert_control_packet_matches {
        ($expr:expr, $expected:expr) => {
            let payload = $expr.next().await.unwrap()?.into_data();
            let mut bytes = BytesMut::from(&payload[..]);
            let control_packet = ControlPacketV2Codec::new().decode(&mut bytes)?.unwrap();
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
                remote_port_end: 4199,
                periodic_cleanup_interval: 2 << 30,
                periodic_ping_interval: 2 << 30,
            }
        );
        let store = Arc::new(Store::new(config.remote_port_start..config.remote_port_end));

        let store_ = store.clone();
        tokio::spawn(async move {
            run(
                &CONFIG,
                store_,
            )
            .await.join_next().await;
        });

        // setup proxy client
        // --- generate valid jwt
        let token = make_jwt("supersecret", CDuration::minutes(10), "127.0.0.1".to_string())?;

        // --- handshake
        let url = Url::parse(&format!("ws://localhost:{}/tunnel", control_port))?;
        let (mut websocket, _) = connect_async(url).await.expect("failed to connect");

        let endpoint_claims = vec![EndpointClaim {
            protocol: Protocol::UDP,
            local_port: 0,
            remote_port: 0,
        }];
        send_client_hello(&mut websocket, token, endpoint_claims).await?;
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
        remote.connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port)).await.unwrap();
        wait!();

        remote
            .send(b"some bytes")
            .await
            .expect("failed to send client hello");

        wait!();
        let stream_id = store.get_stream_ids().await[0];

        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Init(stream_id, client_info.endpoints[0].id)
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Data(stream_id, b"some bytes".to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_client_traffic_to_remote() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            remote.connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port)).await.unwrap();

        // send something for remote stream to be registerd to store
        remote
            .send(b"some bytes")
            .await
            .expect("failed to send client hello");

        wait!();
        let stream_id = store.get_stream_ids().await[0];

        let mut codec = ControlPacketV2Codec::new();
        let mut bytes = BytesMut::new();
        codec.encode(ControlPacketV2::Data(stream_id, b"foobarbaz".to_vec()), &mut bytes)?;
        raw_client_ws_sink
            .send(Message::binary(bytes.to_vec()))
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
        remote1.connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port)).await.unwrap();
        // send something for remote stream to be registerd to store
        remote1
            .send(b"some bytes 1")
            .await
            .expect("failed to send client hello");
        wait!();
        let stream_id1 = store.get_stream_ids().await[0];

        let remote2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote2.connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port)).await.unwrap();
        // send something for remote stream to be registerd to store
        remote2
            .send(b"some bytes 2")
            .await
            .expect("failed to send client hello");
        wait!();
        let stream_id2 = store.get_stream_ids().await
            .into_iter()
            .find(|sid| sid != &stream_id1)
            .unwrap();


        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Init(stream_id1, client_info.endpoints[0].id)
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Data(stream_id1, b"some bytes 1".to_vec())
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Init(stream_id2, client_info.endpoints[0].id)
        );
        assert_control_packet_matches!(
            raw_client_ws_stream,
            ControlPacketV2::Data(stream_id2, b"some bytes 2".to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_client_traffic_to_multiple_remote() -> Result<(), Box<dyn std::error::Error>> {
        let (websoket, store, client_info) = launch_proxy_server(CONTROL_PORT).await?;
        let (mut raw_client_ws_sink, mut _raw_client_ws_stream) = websoket.split();

        let remote1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote1.connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port)).await.unwrap();
        // send something for remote stream to be registerd to store
        remote1
            .send(b"some bytes 1")
            .await
            .expect("failed to send client hello");
        wait!();
        let stream_id1 = store.get_stream_ids().await[0];

        let remote2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote2.connect(format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port)).await.unwrap();
        // send something for remote stream to be registerd to store
        remote2
            .send(b"some bytes 2")
            .await
            .expect("failed to send client hello");
        wait!();
        let stream_id2 = store.get_stream_ids().await
            .into_iter()
            .find(|sid| sid != &stream_id1)
            .unwrap();

        let mut codec = ControlPacketV2Codec::new();
        let mut bytes = BytesMut::new();
        codec.encode(ControlPacketV2::Data(stream_id1, b"some message 1".to_vec()), &mut bytes)?;
        raw_client_ws_sink
            .send(Message::binary(bytes.to_vec()))
            .await?;

        let mut codec = ControlPacketV2Codec::new();
        let mut bytes = BytesMut::new();
        codec.encode(ControlPacketV2::Data(stream_id2, b"some message 2".to_vec()), &mut bytes)?;
        raw_client_ws_sink
            .send(Message::binary(bytes.to_vec()))
            .await?;

        assert_socket_bytes_matches!(remote1, b"some message 1");
        assert_socket_bytes_matches!(remote2, b"some message 2");
        Ok(())
    }
}