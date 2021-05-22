use futures::{SinkExt, StreamExt};
use log::*;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error, Message},
};
use url::Url;
use anyhow::Result;

use std::time::Duration;
use tokio::time;

pub use magic_tunnel::{StreamId, ClientHello};

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let url = Url::parse(&"wss://localhost:5000/")?;
    let (mut ws_stream, _ ) = connect_async(url).await.expect("failed to connect");

    info!("WebSocket handshake has been successfully completed");

    let hello = serde_json::to_vec(&ClientHello {
        id: StreamId::generate(),
    }).unwrap_or_default();
    ws_stream.send(Message::Binary(hello)).await?;


    let mut interval = time::interval(Duration::from_millis(1000));
    for i in 0..3 {
        interval.tick().await;

        ws_stream.send(Message::Binary("hello server".into())).await?;
        info!("message sent: {}", i);
    }

    ws_stream.close(None).await?;

    Ok(())
}
