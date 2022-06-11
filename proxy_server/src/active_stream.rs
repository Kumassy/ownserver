use crate::connected_clients::ConnectedClient;
use dashmap::{DashMap, iter::Iter, mapref::one::Ref};
use futures::{channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender}};
use magic_tunnel_lib::StreamId;
use std::{sync::Arc};

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

#[derive(Debug, Clone, Default)]
pub struct ActiveStreams {
    streams: Arc<DashMap<StreamId, ActiveStream>>
}

impl ActiveStreams {
    pub fn get(&self, stream_id: &StreamId) -> Option<Ref<StreamId, ActiveStream>> {
        self.streams.get(stream_id)
    }

    pub fn insert(&self, stream_id: StreamId, active_stream: ActiveStream) -> Option<ActiveStream> {
        self.streams.insert(stream_id, active_stream)
    }

    pub fn remove(&self, stream_id: &StreamId) -> Option<(StreamId, ActiveStream)> {
        self.streams.remove(stream_id)
    }

    pub fn len(&self) -> usize {
        self.streams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    pub fn clear(&self) {
        self.streams.clear()
    }

    pub fn iter(&self) -> Iter<'_, StreamId, ActiveStream>{
        self.streams.iter()
    }
}

// ActiveStreamMap
// map between remote addr -> stream id
// unbounded channel between 
// ActiveStream で tx を同じ Client に属する ActiveStream 間で共有するようにすればOKか

#[derive(Debug, Clone, PartialEq)]
pub enum StreamMessage {
    Data(Vec<u8>),
    TunnelRefused,
    NoClientTunnel,
}
