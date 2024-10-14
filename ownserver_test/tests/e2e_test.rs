use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::UdpSocket;
use ownserver_test::wait;
use ownserver_lib::{EndpointClaim, Protocol};

#[cfg(test)]
mod e2e_tcp_test {
    use super::*;
    use ownserver::proxy_client::RequestType;
    use ownserver_lib::reconnect::ReconnectTokenPayload;
    use ownserver_test::{assert_tcp_socket_bytes_matches, tcp::{get_endpoint_claims_single, launch_proxy_client_reconnect, with_local_server, with_local_server_echoback, with_proxy}, use_ports, PortSet};

    #[tokio::test]
    async fn forward_remote_traffic_to_local(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let [local_port] = use_ports();
        let endpoint_claims = get_endpoint_claims_single(local_port);

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(local_port, |_local_server| async move {
                let mut remote = TcpStream::connect(remote_addr)
                    .await?;
                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");

                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    async fn forward_multiple_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let [local_port] = use_ports();
        let endpoint_claims = get_endpoint_claims_single(local_port);

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(local_port, |_local_server| async move {
                let mut remote = TcpStream::connect(remote_addr.clone())
                    .await
                    .expect("Failed to connect to remote port");
                let mut remote2 = TcpStream::connect(remote_addr.clone())
                    .await
                    .expect("Failed to connect to remote port");
                wait!();

                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");

                remote2.write_all(b"fugapiyo".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote2, b"hello, fugapiyo");

                Ok(())
            }).await;
            Ok(())
        }).await;
        Ok(())
    }

    #[tokio::test]
    async fn refuse_remote_traffic_when_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        let [local_port] = use_ports();
        let endpoint_claims = get_endpoint_claims_single(local_port);

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info.clone();
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(local_port, |_local_server| async move {
                // cancel client
                proxy_client.cancel().await;
                wait!();

                // access remote port
                // we can access remote server because remote_port_for_client remains open
                // even if client has cancelled
                let mut remote = TcpStream::connect(remote_addr)
                    .await?;
                wait!();

                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"");

                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    async fn refuse_remote_traffic_after_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        let [local_port] = use_ports();
        let endpoint_claims = get_endpoint_claims_single(local_port);

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info.clone();
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(local_port, |_local_server| async move {
                let mut remote = TcpStream::connect(remote_addr)
                    .await?;
                wait!();

                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");

                // cancel client
                proxy_client.cancel().await;
                wait!();

                remote.write_all(b"foobar".as_ref()).await?;
                // assert_tcp_socket_bytes_matches!(remote, b"");
                Ok(())
            }).await;

            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    async fn remove_disabled_client_streams() -> Result<(), Box<dyn std::error::Error>> {
        let [local_port] = use_ports();
        let endpoint_claims = get_endpoint_claims_single(local_port);

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info.clone();
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(local_port, |_local_server| async move {
                let mut remote = TcpStream::connect(remote_addr)
                    .await?;
                wait!();

                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");

                let store = proxy_server.store;

                // now 1 client, 1 stream
                assert_eq!(store.len_clients().await, 1);
                assert_eq!(store.len_streams().await, 1);

                // cancel client
                proxy_client.cancel().await;
                wait!();

                // client and stream remains in store
                assert_eq!(store.len_clients().await, 1);
                assert_eq!(store.len_streams().await, 1);

                // need to call cleanup
                store.cleanup().await;
                wait!();
                assert_eq!(store.len_clients().await, 0);
                assert_eq!(store.len_streams().await, 0);

                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    async fn forward_remote_traffic_to_multiple_local(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let [local1, local2] = use_ports();
        let endpoint_claims = vec![
            EndpointClaim {
                protocol: Protocol::TCP,
                local_port: local1,
                remote_port: 0,
            },
            EndpointClaim {
                protocol: Protocol::TCP,
                local_port: local2,
                remote_port: 0,
            },
        ];

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr0 = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            let remote_addr1 = format!("{}:{}", client_info.host, client_info.endpoints[1].remote_port);
            wait!();

            with_local_server(client_info.endpoints[0].local_port, |_local_server0: ownserver_test::LocalServer| async move {
                let mut remote = TcpStream::connect(remote_addr0)
                    .await?;
                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");

                with_local_server(client_info.endpoints[1].local_port, |_local_server1: ownserver_test::LocalServer| async move {
                    let mut remote = TcpStream::connect(remote_addr1)
                        .await?;
                    remote.write_all(b"barbaz".as_ref()).await?;
                    assert_tcp_socket_bytes_matches!(&mut remote, b"hello, barbaz");

                    Ok(())
                }).await;
                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }


    #[tokio::test]
    async fn forward_multiple_remote_traffic_to_multiple_local(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let [local1, local2] = use_ports();
        let endpoint_claims = vec![
            EndpointClaim {
                protocol: Protocol::TCP,
                local_port: local1,
                remote_port: 0,
            },
            EndpointClaim {
                protocol: Protocol::TCP,
                local_port: local2,
                remote_port: 0,
            },
        ];
        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr0 = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            let remote_addr1 = format!("{}:{}", client_info.host, client_info.endpoints[1].remote_port);
            wait!();

            with_local_server(client_info.endpoints[0].local_port, |_local_server0: ownserver_test::LocalServer| async move {
                let mut remote00 = TcpStream::connect(remote_addr0.clone())
                    .await
                    .expect("Failed to connect to remote port");
                let mut remote01 = TcpStream::connect(remote_addr0.clone())
                    .await
                    .expect("Failed to connect to remote port");
                wait!();

                remote00.write_all(b"e0: foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote00, b"hello, e0: foobar");

                remote01.write_all(b"e0: fugapiyo".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote01, b"hello, e0: fugapiyo");


                with_local_server(client_info.endpoints[1].local_port, |_local_server1: ownserver_test::LocalServer| async move {
                    let mut remote10 = TcpStream::connect(remote_addr1.clone())
                        .await
                        .expect("Failed to connect to remote port");
                    let mut remote11 = TcpStream::connect(remote_addr1.clone())
                        .await
                        .expect("Failed to connect to remote port");
                    wait!();

                    remote10.write_all(b"e1: foobar".as_ref()).await?;
                    assert_tcp_socket_bytes_matches!(&mut remote10, b"hello, e1: foobar");

                    remote11.write_all(b"e1: fugapiyo".as_ref()).await?;
                    assert_tcp_socket_bytes_matches!(&mut remote11, b"hello, e1: fugapiyo");
                    Ok(())
                }).await;
                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }


    #[tokio::test]
    async fn forward_remote_huge_traffic_to_local(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let [local_port] = use_ports();
        let endpoint_claims = get_endpoint_claims_single(local_port);

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server_echoback(local_port, |_local_server| async move {
                let payload: [u8; 16384] = {
                    let mut unique_bytes = [0; 16384];
                    for (i, byte) in unique_bytes.iter_mut().enumerate() {
                        *byte = i as u8;
                    }
                    unique_bytes
                };

                let mut remote = TcpStream::connect(remote_addr)
                    .await?;
                remote.write_all(&payload).await?;


                let mut total_bytes_read = 0;
                let mut received = Vec::new();
                while total_bytes_read < 16384 {
                    let mut buf = [0; 1024];
                    let n = remote.read(&mut buf).await?;
                    total_bytes_read += n;
                    received.extend_from_slice(&buf[..n]);

                    if n == 0 {
                        break
                    }
                }
                assert_eq!(total_bytes_read, 16384);  
                assert_eq!(received, payload);  
                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    async fn reconnect_client(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let [local_port] = use_ports();
        let port_set = PortSet::new();
        let control_port = port_set.control_port;
        let endpoint_claims = get_endpoint_claims_single(local_port);

        with_proxy(port_set, RequestType::NewClient { endpoint_claims }, |_token_server, proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info.clone();
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(local_port, |_local_server| async move {
                let mut remote = TcpStream::connect(remote_addr)
                    .await?;
                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");


                // cancel client
                proxy_client.cancel().await;
                wait!();

                // reconnect
                let token = (ReconnectTokenPayload {
                    client_id: client_info.client_id,
                }).into_token("supersecret")?;
                let _proxy_client_new = launch_proxy_client_reconnect(proxy_client.store, control_port, token, client_info.host, RequestType::Reconnect).await?;
                wait!();

                assert_eq!(proxy_server.store.len_clients().await, 1);
                assert_eq!(proxy_server.store.len_streams().await, 1);

            
                remote.write_all(b"fugapiyo".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, fugapiyo");
                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }
}



#[cfg(test)]
mod e2e_udp_test {
    use ownserver::proxy_client::RequestType;
    use ownserver_test::{assert_udp_socket_bytes_matches, udp::{get_endpoint_claims_single, with_local_server, with_proxy}, use_ports, PortSet};

    use super::*;

    #[tokio::test]
    async fn forward_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let [local_port] = use_ports();
        let endpoint_claims = get_endpoint_claims_single(local_port);

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(local_port, |_local_server| async move {
                let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                    remote.connect(remote_addr).await.unwrap();
                remote.send(b"foobar".as_ref()).await?;
                wait!();

                assert_udp_socket_bytes_matches!(&remote, b"hello, foobar");

                Ok(())
            }).await;
            Ok(())
        }).await;
        Ok(())
    }

    #[tokio::test]
    async fn forward_multiple_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let [local_port] = use_ports();
        let endpoint_claims = get_endpoint_claims_single(local_port);

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(local_port, |_local_server| async move {
                let remote1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                remote1.connect(remote_addr.clone()).await.unwrap();
                let remote2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                remote2.connect(remote_addr).await.unwrap();

                remote1.send(b"foobar".as_ref()).await?;
                remote2.send(b"fugapiyo".as_ref()).await?;
                wait!();

                assert_udp_socket_bytes_matches!(&remote1, b"hello, foobar");
                assert_udp_socket_bytes_matches!(&remote2, b"hello, fugapiyo");

                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }


    #[tokio::test]
    async fn forward_remote_traffic_to_multiple_local(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let [local1, local2] = use_ports();
        let endpoint_claims = vec![
            EndpointClaim {
                protocol: Protocol::UDP,
                local_port: local1,
                remote_port: 0,
            },
            EndpointClaim {
                protocol: Protocol::UDP,
                local_port: local2,
                remote_port: 0,
            },
        ];

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr0 = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            let remote_addr1 = format!("{}:{}", client_info.host, client_info.endpoints[1].remote_port);
            wait!();

            with_local_server(client_info.endpoints[0].local_port, |_local_server| async move {
                let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                    remote.connect(remote_addr0).await.unwrap();
                remote.send(b"foobar".as_ref()).await?;
                wait!();

                assert_udp_socket_bytes_matches!(&remote, b"hello, foobar");

                with_local_server(client_info.endpoints[1].local_port, |_local_server| async move {
                    let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                        remote.connect(remote_addr1).await.unwrap();
                    remote.send(b"barbaz".as_ref()).await?;
                    wait!();
    
                    assert_udp_socket_bytes_matches!(&remote, b"hello, barbaz");
    
                    Ok(())
                }).await;
                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }


    #[tokio::test]
    async fn forward_multiple_remote_traffic_to_multiple_local(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let [local1, local2] = use_ports();
        let endpoint_claims = vec![
            EndpointClaim {
                protocol: Protocol::UDP,
                local_port: local1,
                remote_port: 0,
            },
            EndpointClaim {
                protocol: Protocol::UDP,
                local_port: local2,
                remote_port: 0,
            },
        ];

        with_proxy(PortSet::new(), RequestType::NewClient { endpoint_claims }, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr0 = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            let remote_addr1 = format!("{}:{}", client_info.host, client_info.endpoints[1].remote_port);
            wait!();

            with_local_server(client_info.endpoints[0].local_port, |_local_server0: ownserver_test::LocalServer| async move {
                let remote00 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                    remote00.connect(remote_addr0.clone()).await.unwrap();
                let remote01 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                    remote01.connect(remote_addr0.clone()).await.unwrap();
                

                remote00.send(b"e0: foobar".as_ref()).await?;
                remote01.send(b"e0: barbaz".as_ref()).await?;
                wait!();

                assert_udp_socket_bytes_matches!(&remote00, b"hello, e0: foobar");
                assert_udp_socket_bytes_matches!(&remote01, b"hello, e0: barbaz");

                with_local_server(client_info.endpoints[1].local_port, |_local_server1: ownserver_test::LocalServer| async move {
                    let remote10 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                        remote10.connect(remote_addr1.clone()).await.unwrap();
                    let remote11 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                        remote11.connect(remote_addr1.clone()).await.unwrap();
                    

                    remote10.send(b"e1: foobar".as_ref()).await?;
                    remote11.send(b"e1: barbaz".as_ref()).await?;
                    wait!();

                    assert_udp_socket_bytes_matches!(&remote10, b"hello, e1: foobar");
                    assert_udp_socket_bytes_matches!(&remote11, b"hello, e1: barbaz");

                    Ok(())
                }).await;

                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }
}