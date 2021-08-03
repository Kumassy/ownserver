use magic_tunnel_lib::StreamId;
use crate::connected_clients::ConnectedClient;
use dashmap::DashMap;
use futures::channel::mpsc::{UnboundedSender, UnboundedReceiver, unbounded};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ActiveStream {
    pub id: StreamId,
    pub client: ConnectedClient,
    pub tx: UnboundedSender<StreamMessage>,
}

impl ActiveStream {
    pub fn new(client: ConnectedClient) -> (Self, UnboundedReceiver<StreamMessage>) {
        let (tx, rx) = unbounded();
        (
            ActiveStream {
                id: StreamId::generate(),
                client,
                tx,
            },
            rx,
        )
    }
}

pub type ActiveStreams = Arc<DashMap<StreamId, ActiveStream>>;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamMessage {
    Data(Vec<u8>),
    TunnelRefused,
    NoClientTunnel,
}