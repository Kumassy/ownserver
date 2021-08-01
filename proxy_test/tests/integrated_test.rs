// integrated server test
use std::sync::{Arc, RwLock};
use std::time::Duration;
use std::collections::HashMap;
use dashmap::DashMap;
use lazy_static::lazy_static;
use tokio_tungstenite::{
    connect_async, WebSocketStream, MaybeTlsStream,
    tungstenite::Message,
};
use tokio::net::{TcpStream, TcpListener};
use tokio::sync::Mutex;
use url::Url;
use futures::{StreamExt, SinkExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use magic_tunnel_lib::{ControlPacket, ClientId};
use magic_tunnel_server::{
    proxy_server,
    active_stream::ActiveStreams as ActiveStreamsServer,
    connected_clients::Connections,
    remote::{
        HTTP_TUNNEL_REFUSED_RESPONSE,
        HTTP_ERROR_LOCATING_HOST_RESPONSE,
        CancelHander,   
    },
    port_allocator::PortAllocator,
};
use magic_tunnel_client::{proxy_client, ActiveStreams as ActiveStreamsClient};

#[cfg(test)]
mod proxy_client_server_test {
    use super::*;
    use serial_test::serial;

    // macro_rules! assert_control_packet_type_matches {
    //     ($expr:expr, $pat:pat) => {
    //         let payload = $expr.next().await.unwrap()?.into_data();
    //         let control_packet = ControlPacket::deserialize(&payload)?;
    //         assert!(matches!(control_packet, $pat));
    //     }
    // }

    // macro_rules! assert_control_packet_matches {
    //     ($expr:expr, $expected:expr) => {
    //         let payload = $expr.next().await.unwrap()?.into_data();
    //         let control_packet = ControlPacket::deserialize(&payload)?;
    //         assert_eq!(control_packet, $expected);
    //     }
    // }

    macro_rules! assert_socket_bytes_matches {
        ($read:expr, $expected:expr) => {
            let mut buf = [0; 4*1024];
            let n = $read.read(&mut buf).await.expect("failed to read data from socket");
            let data = buf[..n].to_vec();

            assert_eq!(data, $expected);
        }
    }

    async fn launch_proxy_server(control_port: u16, remote_port: u16) -> Result<ActiveStreamsServer, Box<dyn std::error::Error>> {
        lazy_static! {
            pub static ref CONNECTIONS: Connections = Connections::new();
            pub static ref ACTIVE_STREAMS_SERVER: ActiveStreamsServer = Arc::new(DashMap::new());
        }
        // we must clear CONNECTIONS, ACTIVE_STREAMS
        // because they are shared across test
        Connections::clear(&CONNECTIONS);
        ACTIVE_STREAMS_SERVER.clear();
        let alloc = Arc::new(Mutex::new(PortAllocator::new(4000..4010)));
        let remote_cancellers: Arc<DashMap<ClientId, CancelHander>> = Arc::new(DashMap::new());

        tokio::spawn(async move {
            proxy_server::run(&CONNECTIONS, &ACTIVE_STREAMS_SERVER, alloc, remote_cancellers, control_port, remote_port).await;
        });

        Ok(ACTIVE_STREAMS_SERVER.clone())
    }

    async fn launch_proxy_client(control_port: u16, remote_port: u16, local_port: u16) -> Result<ActiveStreamsClient, Box<dyn std::error::Error>> {
        lazy_static! {
            pub static ref ACTIVE_STREAMS_CLIENT: ActiveStreamsClient = Arc::new(RwLock::new(HashMap::new()));
        }
        // we must clear ACTIVE_STREAMS
        ACTIVE_STREAMS_CLIENT.write().unwrap().clear();

        tokio::spawn(async move {
            proxy_client::run(&ACTIVE_STREAMS_CLIENT, control_port, remote_port, local_port).await.expect("failed to launch proxy_client");
        });

        Ok(ACTIVE_STREAMS_CLIENT.clone())
    }

    async fn launch_local_server(local_port: u16) {
        let local_server = async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", local_port)).await.unwrap();

            loop {
                let (mut socket, _) = listener.accept().await.expect("No connections to accept");

                tokio::spawn(async move {
                    let mut buf = [0; 4*1024];
                    let n = socket.read(&mut buf).await.expect("failed to read data from socket");
                    if n == 0 {
                        return;
                    }

                    let mut msg = b"hello, ".to_vec();
                    msg.append(&mut buf[..n].to_vec());
                    socket.write_all(&msg).await.expect("failed to write packet data to local tcp socket");
                });
            }
        };
        tokio::spawn(local_server);
    }

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let remote_port: u16 = 8080;
        let local_port: u16 = 3000;

        let active_streams_server = launch_proxy_server(control_port, remote_port).await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        launch_local_server(local_port).await;
        let active_streams_client = launch_proxy_client(control_port, remote_port, local_port).await?;
        // wait until client is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(active_streams_server.iter().count(), 0, "active_streams should be empty until remote connection established");
        assert_eq!(active_streams_client.read().unwrap().iter().count(), 0, "active_streams should be empty until remote connection established");

        // access remote port
        let mut remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await.expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(active_streams_server.iter().count(), 1, "remote socket should be accepted and registered");
        assert_eq!(active_streams_client.read().unwrap().iter().count(), 1, "remote socket should be forwared through control packet");

        remote.write_all(&b"foobar".to_vec()).await?;
        assert_socket_bytes_matches!(remote, b"hello, foobar");

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let remote_port: u16 = 8080;
        let local_port: u16 = 3000;

        let active_streams_server = launch_proxy_server(control_port, remote_port).await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        launch_local_server(local_port).await;
        let active_streams_client = launch_proxy_client(control_port, remote_port, local_port).await?;
        // wait until client is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(active_streams_server.iter().count(), 0, "active_streams should be empty until remote connection established");
        assert_eq!(active_streams_client.read().unwrap().iter().count(), 0, "active_streams should be empty until remote connection established");

        // access remote port
        let mut remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await.expect("Failed to connect to remote port");
        let mut remote2 = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await.expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(active_streams_server.iter().count(), 2, "remote socket should be accepted and registered");
        assert_eq!(active_streams_client.read().unwrap().iter().count(), 2, "remote socket should be forwared through control packet");

        remote.write_all(&b"foobar".to_vec()).await?;
        assert_socket_bytes_matches!(remote, b"hello, foobar");

        remote2.write_all(&b"fugapiyo".to_vec()).await?;
        assert_socket_bytes_matches!(remote2, b"hello, fugapiyo");

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_local_server_not_running() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let remote_port: u16 = 8080;
        let local_port: u16 = 3000;

        let active_streams_server = launch_proxy_server(control_port, remote_port).await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        let active_streams_client = launch_proxy_client(control_port, remote_port, local_port).await?;
        // wait until client is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(active_streams_server.iter().count(), 0, "active_streams should be empty until remote connection established");
        assert_eq!(active_streams_client.read().unwrap().iter().count(), 0, "active_streams should be empty until remote connection established");

        // access remote port
        let mut remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await.expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(active_streams_server.iter().count(), 0, "active_streams should be empty. remote stream is once registered, but immediately removed at the exit of tunnel_to_stream");
        assert_socket_bytes_matches!(remote, HTTP_TUNNEL_REFUSED_RESPONSE);

        // first write operation success because still in loop of process_tcp_stream
        remote.write_all(&b"foobar".to_vec()).await?; 
        remote.flush().await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        // at the second write operation, proxy_server send FIN
        remote.write_all(&b"foobar".to_vec()).await?; 
        remote.flush().await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(remote.write_all(&b"foobar".to_vec()).await.is_err(), "third operation must fail because proxy_server release tcp connection.");
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_client_not_connected() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let remote_port: u16 = 8080;

        let active_streams_server = launch_proxy_server(control_port, remote_port).await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(active_streams_server.iter().count(), 0, "active_streams should be empty until remote connection established");

        // access remote port
        let mut remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await.expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(active_streams_server.iter().count(), 0, "active_streams should be empty. remote stream is never registered.");
        assert_socket_bytes_matches!(remote, HTTP_ERROR_LOCATING_HOST_RESPONSE);

        // at the first write operation, proxy_server send FIN
        remote.write_all(&b"foobar".to_vec()).await?; 
        remote.flush().await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
       
        assert!(remote.write_all(&b"foobar".to_vec()).await.is_err(), "second operation must fail because proxy_server release tcp connection.");
        Ok(())
    }
}