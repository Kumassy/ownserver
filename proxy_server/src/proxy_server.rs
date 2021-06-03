pub use magic_tunnel_lib::{StreamId, ClientHello};
use futures::{SinkExt, StreamExt};
// use log::*;
use std::net::{SocketAddr, IpAddr};
use tokio::task::JoinHandle;
use tracing::{info, Instrument};
use tracing_subscriber;
use warp::{Filter, Rejection, ws::{Ws, WebSocket, Message}};
use std::convert::Infallible;

pub fn spawn<A: Into<SocketAddr>>(addr: A) -> JoinHandle<()> {
    let health_check = warp::get().and(warp::path("health_check")).map(|| {
        tracing::debug!("Health Check #2 triggered");
        "ok"
    });
    let client_conn = warp::path("tunnel").and(client_addr()).and(warp::ws()).map(
        move |client_addr: SocketAddr, ws: Ws| {
            ws.on_upgrade(move |w| {
                async move { handle_new_connection(client_addr, w).await }
                    .instrument(tracing::info_span!("handle_websocket"))
            })
        },
    );

    let routes = client_conn.or(health_check);

    // TODO tls https://docs.rs/warp/0.3.1/warp/struct.Server.html#method.tls
    tokio::spawn(warp::serve(routes).run(addr.into()))
}

// fn client_ip() -> impl Filter<Extract = (IpAddr,), Error = Rejection> + Copy {
fn client_addr() -> impl Filter<Extract = (SocketAddr,), Error = Infallible> + Copy {
    warp::any()
        .and(warp::addr::remote())
        .map(
            |remote: Option<SocketAddr>| {
                remote
                    .unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0)))
            },
        )
}

// WebSocketStream 
// async fn try_client_handshake(websocket: &mut WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>) -> Option<ClientHello> {
async fn try_client_handshake(websocket: &mut WebSocket) -> Option<ClientHello> {
    let client_hello_data = match websocket.next().await {
        Some(Ok(msg)) if (msg.is_binary() || msg.is_text()) && !msg.as_bytes().is_empty() => {
            msg.into_bytes()
        }
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

async fn handle_new_connection(peer: SocketAddr, mut websocket: WebSocket) {
    info!("New WebSocket connection: {}", peer);

    let client_hello = try_client_handshake(&mut websocket).await.expect("failed to verify client hello");
    info!("verify client hello {:?}", client_hello);

    while let Some(msg) = websocket.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            _ => {
                tracing::debug!("client websocket has ended");
                return;
            }
        };
        info!("message received: {:?}", msg);
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let addr: SocketAddr = ([0, 0, 0, 0], 5000).into();
    let handle = spawn(addr);
    info!("Listening on: {}", addr);

    handle.await.unwrap();
}

#[cfg(test)]
mod try_client_handshake_test {
    use super::*;

    macro_rules! test_helper {
        ($send_value:expr, $expect:expr) => {
            let server = warp::ws().map(|ws: Ws| {
                ws.on_upgrade(|mut websocket| {
                    async move {
                        let hello = try_client_handshake(&mut websocket).await;
                        $expect(hello);
                        // make sure $expect to be executed
                        websocket.send(Message::text("ok")).await.expect("send back response");
                    }
                })
            });

            let mut client = warp::test::ws()
                .handshake(server)
                .await
                .expect("handshake");

            client.send($send_value).await;
            client.recv().await.expect("recv");
        };
    }

    #[tokio::test]
    async fn accept_client_hello() {
        let hello = serde_json::to_vec(&ClientHello {
            id: StreamId::generate(),
        }).unwrap_or_default();
        test_helper!(Message::binary(hello), |hello: Option<ClientHello>| {
            assert!(hello.is_some());
            let hello = hello.unwrap();
            assert_eq!(hello.id, StreamId("stream_foo".to_string()));
        });
    }

    #[tokio::test]
    async fn reject_invalid_text_hello() {
        test_helper!(Message::text("foobarbaz".to_string()), |hello: Option<ClientHello>| {
            assert!(hello.is_none());
        });
    }
    #[tokio::test]
    async fn reject_invalid_serialized_hello() {
        let hello = serde_json::to_vec(&"malformed".to_string()).unwrap_or_default();
        test_helper!(Message::binary(hello), |hello: Option<ClientHello>| {
            assert!(hello.is_none());
        });
    }
}