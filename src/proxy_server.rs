pub use magic_tunnel::{StreamId, ClientHello};
use futures::{SinkExt, StreamExt};
// use log::*;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, client_async, WebSocketStream, tungstenite::{Error, Result, Message}};
use tokio::io::{AsyncRead, AsyncWrite};
use std::marker::Unpin;
use tokio::sync::oneshot;
use tracing::info;
use tracing_subscriber;

// WebSocketStream 
// async fn try_client_handshake(websocket: &mut WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>) -> Option<ClientHello> {
async fn try_client_handshake(websocket: &mut WebSocketStream<TcpStream>) -> Option<ClientHello> {
    let client_hello_data = match websocket.next().await {
        Some(Ok(Message::Binary(msg))) => msg,
        _ => {
            tracing::error!("no client init message");
            return None
        },
    };

    let client_hello: ClientHello = match serde_json::from_slice(&client_hello_data) {
        Ok(client_hello) => client_hello,
        _ => {
            tracing::error!("failed to deserialize client hello");
            return None
        }
    };
    Some(client_hello)
}

async fn handle_new_connection(peer: SocketAddr, stream: TcpStream) -> Result<()> {
    let mut ws_stream = accept_async(stream).await.expect("Failed to accept");
    info!("New WebSocket connection: {}", peer);

    let client_hello = try_client_handshake(&mut ws_stream).await.expect("failed to verify client hello");
    info!("verify client hello {:?}", client_hello);

    while let Some(msg) = ws_stream.next().await {
        let msg = msg?;
        info!("message received: {:?}", msg);
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    // pretty_env_logger::init();
    tracing_subscriber::fmt::init();

    let addr = "127.0.0.1:5000";
    let listener = TcpListener::bind(&addr).await.expect("Can't listen");
    info!("Listening on: {}", addr);

    while let Ok((stream, _)) = listener.accept().await {
        let peer = stream.peer_addr().expect("connected streams should have a peer address");
        info!("Peer address: {}", peer);

        tokio::spawn(handle_new_connection(peer, stream));
    }
}

#[cfg(test)]
mod try_client_handshake_test {
    use super::*;

    macro_rules! test_helper {
        // The `tt` (token tree) designator is used for
        // operators and tokens.
        ($send_value:expr, $expect:expr) => {
            let (con_tx, con_rx) = oneshot::channel();
            let (msg_tx, msg_rx) = oneshot::channel();
            let server = async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                con_tx.send(listener.local_addr().unwrap()).unwrap();
                let (connection, _) = listener.accept().await.expect("No connections to accept");
                let stream = accept_async(connection).await;
                let mut stream = stream.expect("Failed to handshake with connection");

                let hello = try_client_handshake(&mut stream).await;
                msg_tx.send(hello).unwrap();
            };
            tokio::spawn(server);

            info!("Waiting for server to be ready");
            let server_addr = con_rx.await.expect("Server not ready");
            let server_port = server_addr.port();

            let tcp = TcpStream::connect(server_addr).await.expect("Failed to connect");
            let url = format!("ws://localhost:{}/", server_port);
            let (mut stream, _) = client_async(url, tcp).await.expect("Client failed to connect");

            stream.send($send_value).await.expect("failed to send client hello");

            let hello = msg_rx.await.expect("Failed to receive messages");

            $expect(hello);
        };
    }


    #[tokio::test]
    async fn accept_client_hello() {
        let hello = serde_json::to_vec(&ClientHello {
            id: StreamId::generate(),
        }).unwrap_or_default();
        test_helper!(Message::Binary(hello), |hello: Option<ClientHello>| {
            assert!(hello.is_some());
            let hello = hello.unwrap();
            assert_eq!(hello.id, StreamId("stream_foo".to_string()));
        });
    }

    #[tokio::test]
    async fn reject_invalid_text_hello() {
        test_helper!(Message::Text("foobarbaz".into()), |hello: Option<ClientHello>| {
            assert!(hello.is_none());
        });
    }
    #[tokio::test]
    async fn reject_invalid_serialized_hello() {
        let hello = serde_json::to_vec(&"malformed".to_string()).unwrap_or_default();
        test_helper!(Message::Binary(hello), |hello: Option<ClientHello>| {
            assert!(hello.is_none());
        });
    }
}