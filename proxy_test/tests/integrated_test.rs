// integrated server test
use dashmap::DashMap;
use lazy_static::lazy_static;
use magic_tunnel_client::{
    proxy_client::{self, ClientInfo},
    ActiveStreams as ActiveStreamsClient,
};
use magic_tunnel_lib::ClientId;
use magic_tunnel_server::{
    active_stream::ActiveStreams as ActiveStreamsServer,
    connected_clients::Connections,
    port_allocator::PortAllocator,
    proxy_server,
    remote::HTTP_TUNNEL_REFUSED_RESPONSE,
    Config,
};
use magic_tunnel_auth::build_routes;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot::{self, Receiver};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use once_cell::sync::OnceCell;

#[cfg(test)]
mod proxy_client_server_test {
    use super::*;
    use serial_test::serial;

    static CONFIG: OnceCell<Config> = OnceCell::new();

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
            let mut buf = [0; 4 * 1024];
            let n = $read
                .read(&mut buf)
                .await
                .expect("failed to read data from socket");
            let data = buf[..n].to_vec();

            assert_eq!(data, $expected);
        };
    }

    async fn launch_token_server() {
        let hosts = vec![
            "127.0.0.1".to_string(),
        ];
        let routes = build_routes("supersecret", hosts);

        tokio::spawn(async move {
            warp::serve(routes).run(([127, 0, 0, 1], 8888)).await;
        });
    }

    async fn launch_proxy_server() -> Result<ActiveStreamsServer, Box<dyn std::error::Error>> {
        lazy_static! {
            pub static ref CONNECTIONS: Connections = Connections::new();
            pub static ref ACTIVE_STREAMS_SERVER: ActiveStreamsServer = Arc::new(DashMap::new());
        }
        // we must clear CONNECTIONS, ACTIVE_STREAMS
        // because they are shared across test
        Connections::clear(&CONNECTIONS);
        ACTIVE_STREAMS_SERVER.clear();

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
        let remote_cancellers: Arc<DashMap<ClientId, CancellationToken>> = Arc::new(DashMap::new());

        tokio::spawn(async move {
            proxy_server::run(
                &CONFIG,
                &CONNECTIONS,
                &ACTIVE_STREAMS_SERVER,
                alloc,
                remote_cancellers,
            )
            .await.unwrap();
        });

        Ok(ACTIVE_STREAMS_SERVER.clone())
    }

    async fn launch_proxy_client(
        control_port: u16,
        local_port: u16,
        cancellation_token: CancellationToken,
    ) -> Result<(ActiveStreamsClient, Receiver<ClientInfo>), Box<dyn std::error::Error>> {
        lazy_static! {
            pub static ref ACTIVE_STREAMS_CLIENT: ActiveStreamsClient =
                Arc::new(RwLock::new(HashMap::new()));
        }
        // we must clear ACTIVE_STREAMS
        ACTIVE_STREAMS_CLIENT.write().unwrap().clear();

        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (client_info, handle) =
                proxy_client::run(&ACTIVE_STREAMS_CLIENT, control_port, local_port, "http://127.0.0.1:8888/request_token", cancellation_token)
                    .await
                    .expect("failed to launch proxy_client");
            tx.send(client_info).unwrap();

            handle.await.unwrap().unwrap();
        });

        Ok((ACTIVE_STREAMS_CLIENT.clone(), rx))
    }

    async fn launch_local_server(local_port: u16) {
        let local_server = async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", local_port))
                .await
                .unwrap();

            loop {
                let (mut socket, _) = listener.accept().await.expect("No connections to accept");

                tokio::spawn(async move {
                    loop {
                        let mut buf = [0; 4 * 1024];
                        let n = socket
                            .read(&mut buf)
                            .await
                            .expect("failed to read data from socket");
                        if n == 0 {
                            return;
                        }

                        let mut msg = b"hello, ".to_vec();
                        msg.append(&mut buf[..n].to_vec());
                        socket
                            .write_all(&msg)
                            .await
                            .expect("failed to write packet data to local tcp socket");
                    }
                });
            }
        };
        tokio::spawn(local_server);
    }

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let local_port: u16 = 3000;
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let active_streams_server = launch_proxy_server().await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        launch_local_server(local_port).await;
        let (active_streams_client, client_info) =
            launch_proxy_client(control_port, local_port, cancellation_token).await?;
        let remote_addr = client_info.await?.remote_addr;
        // wait until client is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams_server.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        let mut remote = TcpStream::connect(remote_addr)
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams_server.iter().count(),
            1,
            "remote socket should be accepted and registered"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            1,
            "remote socket should be forwared through control packet"
        );

        remote.write_all(&b"foobar".to_vec()).await?;
        assert_socket_bytes_matches!(remote, b"hello, foobar");

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let local_port: u16 = 3000;
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let active_streams_server = launch_proxy_server().await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        launch_local_server(local_port).await;
        let (active_streams_client, client_info) =
            launch_proxy_client(control_port, local_port, cancellation_token).await?;
        let remote_addr = client_info.await?.remote_addr;
        // wait until client is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams_server.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        let mut remote = TcpStream::connect(remote_addr.clone())
            .await
            .expect("Failed to connect to remote port");
        let mut remote2 = TcpStream::connect(remote_addr.clone())
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams_server.iter().count(),
            2,
            "remote socket should be accepted and registered"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            2,
            "remote socket should be forwared through control packet"
        );

        remote.write_all(&b"foobar".to_vec()).await?;
        assert_socket_bytes_matches!(remote, b"hello, foobar");

        remote2.write_all(&b"fugapiyo".to_vec()).await?;
        assert_socket_bytes_matches!(remote2, b"hello, fugapiyo");

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_local_server_not_running(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let local_port: u16 = 3000;
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let active_streams_server = launch_proxy_server().await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        let (active_streams_client, client_info) =
            launch_proxy_client(control_port, local_port, cancellation_token).await?;
        let remote_addr = client_info.await?.remote_addr;
        // wait until client is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams_server.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        let mut remote = TcpStream::connect(remote_addr)
            .await
            .expect("Failed to connect to remote port");
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
        assert!(
            remote.write_all(&b"foobar".to_vec()).await.is_err(),
            "third operation must fail because proxy_server release tcp connection."
        );
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_remote_process_not_running(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let remote_port: u16 = 8080;

        launch_token_server().await;
        let active_streams_server = launch_proxy_server().await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams_server.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        let remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await;
        assert!(remote.is_err());

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let local_port: u16 = 3000;
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let active_streams_server = launch_proxy_server().await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        launch_local_server(local_port).await;
        let (active_streams_client, client_info) =
            launch_proxy_client(control_port, local_port, cancellation_token.clone()).await?;
        let remote_addr = client_info.await?.remote_addr;
        // wait until client is ready
        tokio::time::sleep(Duration::from_secs(3)).await;
        
        // cancel client
        cancellation_token.cancel();

        assert_eq!(
            active_streams_server.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        // we can access remote server because remote_port_for_client remains open
        // even if client has cancelled
        let mut remote = TcpStream::connect(remote_addr)
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams_server.iter().count(),
            0,
            "server stream should be empty after client has exited"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            0,
            "client stream should be empty after client has exited"
        );

        remote.write_all(&b"foobar".to_vec()).await?;
        assert_socket_bytes_matches!(remote, b"HTTP/1.1 500\r\nContent-Length: 27\r\n\r\nError: Error finding tunnel");

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_after_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        let control_port: u16 = 5000;
        let local_port: u16 = 3000;
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let active_streams_server = launch_proxy_server().await?;
        // wait until server is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        launch_local_server(local_port).await;
        let (active_streams_client, client_info) =
            launch_proxy_client(control_port, local_port, cancellation_token.clone()).await?;
        let remote_addr = client_info.await?.remote_addr;
        // wait until client is ready
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams_server.iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            0,
            "active_streams should be empty until remote connection established"
        );

        // access remote port
        let mut remote = TcpStream::connect(remote_addr)
            .await
            .expect("Failed to connect to remote port");
        // wait until remote access has registered to ACTIVE_STREAMS
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(
            active_streams_server.iter().count(),
            1,
            "remote socket should be accepted and registered"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            1,
            "remote socket should be forwared through control packet"
        );

        remote.write_all(&b"foobar".to_vec()).await?;
        assert_socket_bytes_matches!(remote, b"hello, foobar");

        // cancel client
        cancellation_token.cancel();
        tokio::time::sleep(Duration::from_secs(6)).await;

        remote.write_all(&b"foobar".to_vec()).await?;
        assert_socket_bytes_matches!(remote, b"HTTP/1.1 404\r\nContent-Length: 23\r\n\r\nError: Tunnel Not Found");

        assert_eq!(
            active_streams_server.iter().count(),
            0,
            "server stream should be empty after client has exited"
        );
        assert_eq!(
            active_streams_client.read().unwrap().iter().count(),
            1,
            "client stream should remain same after client has exited"
        );

        Ok(())
    }
}
