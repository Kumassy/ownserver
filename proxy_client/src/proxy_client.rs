use futures::{SinkExt, StreamExt};
use futures::channel::mpsc::{unbounded, UnboundedSender};
use log::*;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error, Message},
};
use url::Url;
use anyhow::Result;
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time;

pub use magic_tunnel_lib::{StreamId, ClientHello};


pub type ActiveStreams = Arc<RwLock<HashMap<StreamId, UnboundedSender<StreamMessage>>>>;

lazy_static::lazy_static! {
    pub static ref ACTIVE_STREAMS:ActiveStreams = Arc::new(RwLock::new(HashMap::new()));
}

#[derive(Debug, Clone)]
pub enum StreamMessage {
    Data(Vec<u8>),
    Close,
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let url = Url::parse(&"wss://localhost:5000/tunnel")?;
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
