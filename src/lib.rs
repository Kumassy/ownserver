use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct StreamId(pub String);

impl StreamId {
    pub fn generate() -> Self {
        StreamId("stream_foo".to_string())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientHello {
    pub id: StreamId,
}