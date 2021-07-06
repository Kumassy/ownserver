use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use magic_tunnel_lib::StreamId;
use futures::channel::mpsc::UnboundedSender;

pub type ActiveStreams = Arc<RwLock<HashMap<StreamId, UnboundedSender<StreamMessage>>>>;

#[derive(Debug, Clone)]
pub enum StreamMessage {
    Data(Vec<u8>),
    Close,
}
pub mod error;
pub mod local;
pub mod proxy_client;

