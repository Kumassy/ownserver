pub use magic_tunnel_lib::{StreamId, ClientId, ClientHello, ServerHello};
use futures::{SinkExt, StreamExt, Stream, Sink, AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt};
// use log::*;
use std::net::{SocketAddr, IpAddr};
use tokio::task::JoinHandle;
use tracing::{info, Instrument};
use tracing_subscriber;
use warp::{Filter, Rejection, ws::{Ws, WebSocket, Message}, Error as WarpError};
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

// async fn try_client_handshake(websocket: &mut WebSocket) -> Option<ClientHello> {
async fn try_client_handshake(websocket: &mut (impl Unpin + Stream<Item=Result<Message, WarpError>>)) -> Option<ClientHello> {
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

// async fn respond_with_server_hello(websocket: &mut (impl Unpin + Sink<Message>)) -> Result<(), WarpError> 
async fn respond_with_server_hello<T>(websocket: &mut T) -> Result<(), T::Error> 
    where T: Unpin + Sink<Message> {
    // Send server hello success
    let client_id = ClientId::generate();
    let data = serde_json::to_vec(&ServerHello::Success {
        client_id: client_id.clone(),
        assigned_port: 12345, // TODO
    })
    .unwrap_or_default();

    websocket.send(Message::binary(data)).await?;
    // if let Err(error) = send_result {
    //     tracing::info!("failed to send server hello: {}", error);
    //     return None;
    // }

    tracing::debug!(
        "new client connected: {:?}",
        client_id,
    );

    Ok(())
}

async fn handle_new_connection(peer: SocketAddr, mut websocket: WebSocket) {
    info!("New WebSocket connection: {}", peer);

    let client_hello = try_client_handshake(&mut websocket).await.expect("failed to verify client hello");
    info!("verify client hello {:?}", client_hello);

    respond_with_server_hello(&mut websocket).await.expect("failed to send server_hello");

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
    use futures::channel::mpsc;

    #[tokio::test]
    async fn accept_client_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        let hello = serde_json::to_vec(&ClientHello {
            id: ClientId::generate(),
        }).unwrap_or_default();

        tx.send(Ok(Message::binary(hello))).await?;
        let hello = try_client_handshake(&mut rx).await;
        assert!(hello.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_text_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        tx.send(Ok(Message::text("foobarbaz".to_string()))).await?;
        let hello = try_client_handshake(&mut rx).await;
        assert!(hello.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn reject_invalid_serialized_hello() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        let hello = serde_json::to_vec(&"malformed".to_string()).unwrap_or_default();

        tx.send(Ok(Message::binary(hello))).await?;
        let hello = try_client_handshake(&mut rx).await;
        assert!(hello.is_none());
        Ok(())
    }
}


#[cfg(test)]
mod respond_with_server_hello_test {
    use super::*;
    use futures::channel::mpsc;

    #[tokio::test]
    async fn it_works() -> Result<(), Box<dyn std::error::Error>> {
        let (mut tx, mut rx) = mpsc::unbounded();

        respond_with_server_hello(&mut tx).await.expect("failed to write to websocket");

        let server_hello_data = rx.next().await.unwrap().into_bytes();
        let server_hello: ServerHello = serde_json::from_slice(&server_hello_data).expect("server hello is malformed");

        assert!(matches!(server_hello, ServerHello::Success{ .. }));
        Ok(())
    }
}