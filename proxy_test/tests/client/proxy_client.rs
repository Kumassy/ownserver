#[cfg(test)]
mod client_process_control_flow_message_test {
    use super::*;

    use std::time::Duration;
    use futures::{SinkExt, StreamExt};
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use log::*;
    use magic_tunnel_client::{ActiveStreams, proxy_client::process_control_flow_message};
    use magic_tunnel_lib::{ControlPacket, StreamId};
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use tokio::io::{split, AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    async fn setup() -> (
        ActiveStreams,
        UnboundedSender<ControlPacket>,
        UnboundedReceiver<ControlPacket>,
        StreamId,
        u16,
        UnboundedReceiver<Vec<u8>>,
    ) {
        let active_streams = Arc::new(RwLock::new(HashMap::new()));

        let (con_tx, con_rx) = oneshot::channel();
        let (mut msg_tx, msg_rx) = unbounded();

        let server = async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            con_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let (local_tcp, _) = listener.accept().await.expect("No connections to accept");
            let (mut stream, mut sink) = split(local_tcp);

            // constantly write to local tcp so that process_local_tcp don't close
            tokio::spawn(async move {
                loop {
                    sink.write_all(&b"some data".to_vec())
                        .await
                        .expect("failed to write packet data to local tcp socket");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });

            // read local tcp to capture output of forward_to_local_tcp
            tokio::spawn(async move {
                let mut buf = [0; 4 * 1024];
                loop {
                    let n = stream
                        .read(&mut buf)
                        .await
                        .expect("failed to read data from socket");
                    if n == 0 {
                        info!("done reading from client stream");
                        return;
                    }
                    let data = buf[..n].to_vec();
                    msg_tx.send(data).await.expect("failed to write to msg_tx");
                }
            });
        };
        tokio::spawn(server);

        let stream_id = StreamId::generate();
        let local_port = con_rx.await.expect("failed to get local port");
        let (tunnel_tx, tunnel_rx) = unbounded::<ControlPacket>();

        (
            active_streams,
            tunnel_tx,
            tunnel_rx,
            stream_id,
            local_port,
            msg_rx,
        )
    }

    fn assert_active_streams_len(active_streams: &ActiveStreams, expected: usize) {
        assert_eq!(
            active_streams
                .read()
                .expect("failed to obtain read lock over ActiveStreams")
                .len(),
            expected
        );
    }

    #[tokio::test]
    async fn handle_control_packet_init() {
        let (active_streams, tunnel_tx, _tunnel_rx, stream_id, local_port, _) = setup().await;

        // ensure active_streams has registered
        assert_eq!(
            0,
            active_streams
                .read()
                .expect("failed to obtain read lock over ActiveStreams")
                .len()
        );
        let _ = process_control_flow_message(
            active_streams.clone(),
            tunnel_tx,
            ControlPacket::Init(stream_id.clone()).serialize(),
            local_port,
        )
        .await
        .expect("failed to decode packet");
        assert_active_streams_len(&active_streams, 1);
    }

    #[tokio::test]
    async fn handle_control_packet_ping() {
        let (active_streams, tunnel_tx, mut tunnel_rx, _, local_port, _) = setup().await;

        let _ = process_control_flow_message(
            active_streams.clone(),
            tunnel_tx,
            ControlPacket::Ping.serialize(),
            local_port,
        )
        .await
        .expect("failed to decode packet");

        // ping packet should be sent to proxy server
        assert_eq!(tunnel_rx.next().await, Some(ControlPacket::Ping));
    }

    // TODO: handle ControlPacket::Refused
    #[tokio::test]
    #[should_panic(expected = "ControlPacket::Refused may be unimplemented")]
    async fn handle_control_packet_refused() {
        let (active_streams, tunnel_tx, _tunnel_rx, stream_id, local_port, _) = setup().await;

        let _ = process_control_flow_message(
            active_streams.clone(),
            tunnel_tx,
            ControlPacket::Refused(stream_id).serialize(),
            local_port,
        )
        .await
        .expect("ControlPacket::Refused may be unimplemented");
    }

    #[tokio::test]
    async fn handle_control_packet_end() {
        let (active_streams, tunnel_tx, _tunnel_rx, stream_id, local_port, _) = setup().await;

        // set up local connection
        let _ = process_control_flow_message(
            active_streams.clone(),
            tunnel_tx.clone(),
            ControlPacket::Init(stream_id.clone()).serialize(),
            local_port,
        )
        .await
        .expect("failed to decode packet");
        assert_active_streams_len(&active_streams, 1);

        // terminate local connection
        let _ = process_control_flow_message(
            active_streams.clone(),
            tunnel_tx,
            ControlPacket::End(stream_id.clone()).serialize(),
            local_port,
        )
        .await
        .expect("failed to decode packet");

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_active_streams_len(&active_streams, 1);

        tokio::time::sleep(Duration::from_secs(8)).await;
        assert_active_streams_len(&active_streams, 0);
    }

    #[tokio::test]
    async fn handle_control_packet_data() {
        let (active_streams, tunnel_tx, _tunnel_rx, stream_id, local_port, mut msg_rx) = setup().await;

        // set up local connection
        let _ = process_control_flow_message(
            active_streams.clone(),
            tunnel_tx.clone(),
            ControlPacket::Init(stream_id.clone()).serialize(),
            local_port,
        )
        .await
        .expect("failed to decode packet");

        // receive data from server
        let _ = process_control_flow_message(
            active_streams.clone(),
            tunnel_tx,
            ControlPacket::Data(stream_id.clone(), b"some message 1".to_vec()).serialize(),
            local_port,
        )
        .await
        .expect("failed to decode packet");

        // data should be sent to local TcpStream
        assert_eq!(msg_rx.next().await, Some(b"some message 1".to_vec()));
    }

    #[tokio::test]
    async fn handle_control_packet_data_refused() {
        let (active_streams, tunnel_tx, mut tunnel_rx, stream_id, _, _) = setup().await;

        // receive data from server
        let _ = process_control_flow_message(
            active_streams.clone(),
            tunnel_tx,
            ControlPacket::Data(stream_id.clone(), b"some message 2".to_vec()).serialize(),
            25565,
        )
        .await
        .expect("failed to decode packet");

        // connection refused should be sent to proxy server
        assert_eq!(
            tunnel_rx.next().await,
            Some(ControlPacket::Refused(stream_id.clone()))
        );
    }
}

#[cfg(test)]
mod client_send_client_hello_test {
    use super::*;
    use futures::{channel::mpsc, StreamExt};
    use magic_tunnel_client::proxy_client::send_client_hello;
    use magic_tunnel_lib::ClientHello;

    #[tokio::test]
    async fn it_sends_client_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        send_client_hello(&mut tx, "footoken".to_string())
            .await
            .expect("failed to write to websocket");

        let client_hello_data = rx.next().await.unwrap().into_data();
        let _client_hello: ClientHello =
            serde_json::from_slice(&client_hello_data).expect("client hello is malformed");

        Ok(())
    }
}

#[cfg(test)]
mod client_verify_server_hello_test {
    use super::*;
    use futures::{channel::mpsc, SinkExt};
    use magic_tunnel_client::{proxy_client::{verify_server_hello, ClientInfo}, error::Error};
    use magic_tunnel_lib::{ClientId, ServerHello};
    use tokio_tungstenite::{
        tungstenite::{Error as WsError, Message},
    };

    #[tokio::test]
    async fn it_accept_server_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        let cid = ClientId::generate();
        let hello = serde_json::to_vec(&ServerHello::Success {
            client_id: cid.clone(),
            remote_addr: "foo.bar.local:256".to_string(),
        })
        .unwrap_or_default();
        tx.send(Ok(Message::binary(hello))).await?;

        let client_info = verify_server_hello(&mut rx)
            .await
            .expect("unexpected server hello error");
        let ClientInfo {
            client_id,
            remote_addr,
        } = client_info;
        assert_eq!(cid, client_id);
        assert_eq!("foo.bar.local:256".to_string(), remote_addr);

        Ok(())
    }

    #[tokio::test]
    async fn returns_errors_when_websocket_yields_nothing() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut tx, mut rx) = mpsc::unbounded();

        tx.disconnect();

        let server_hello = verify_server_hello(&mut rx)
            .await
            .err()
            .expect("server hello is unexpectedly correct");
        assert!(matches!(server_hello, Error::NoResponseFromServer));

        Ok(())
    }

    #[tokio::test]
    async fn returns_errors_when_server_hello_is_invalid() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut tx, mut rx) = mpsc::unbounded();

        let hello = serde_json::to_vec(&"hello server").unwrap_or_default();
        tx.send(Ok(Message::binary(hello))).await?;

        let server_hello = verify_server_hello(&mut rx)
            .await
            .err()
            .expect("server hello is unexpectedly correct");
        assert!(matches!(server_hello, Error::ServerReplyInvalid));

        Ok(())
    }

    #[tokio::test]
    async fn returns_errors_when_websocket_error() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        tx.send(Err(WsError::AlreadyClosed)).await?;

        let server_hello = verify_server_hello(&mut rx)
            .await
            .err()
            .expect("server hello is unexpectedly correct");
        assert!(matches!(server_hello, Error::WebSocketError(_)));

        Ok(())
    }
}