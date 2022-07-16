use dashmap::DashMap;
use dashmap::mapref::one::{Ref, RefMut};
use futures::channel::mpsc::UnboundedSender;
use magic_tunnel_lib::StreamId;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub enum StreamMessage {
    Data(Vec<u8>),
    Close,
}
pub mod error;
pub mod local;
pub mod localudp;
pub mod proxy_client;


pub type LocalStream = UnboundedSender<StreamMessage>;
#[derive(Debug, Default)]
pub struct Store {
    streams: DashMap<StreamId, LocalStream>
}

impl Store {
    pub fn add_stream(&self, stream_id: StreamId, stream: LocalStream) {
        self.streams.insert(stream_id, stream);
    }

    pub fn remove_stream(&self, stream_id: &StreamId) -> Option<(StreamId, LocalStream)> {
        self.streams.remove(stream_id)
    }

    pub fn has_stream(&self, stream_id: &StreamId) -> bool {
        self.streams.contains_key(stream_id)
    }

    pub fn get_stream(&self, stream_id: &StreamId) -> Option<Ref<StreamId, LocalStream>> {
        self.streams.get(stream_id)
    }

    pub fn get_mut_stream(&self, stream_id: &StreamId) -> Option<RefMut<StreamId, LocalStream>> {
        self.streams.get_mut(stream_id)
    }

    pub fn len_stream(&self) -> usize {
        self.streams.len()
    }
}
