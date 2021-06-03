use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct StreamId(pub String);

impl StreamId {
    pub fn generate() -> Self {
        StreamId("stream_foo".to_string())
    }
    pub fn to_string(&self) -> String {
        format!(
            "stream_{}",
            base64::encode_config(&self.0, base64::URL_SAFE_NO_PAD)
        )
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientHello {
    pub id: StreamId,
}

#[derive(Debug, Clone)]
pub enum ControlPacket {
    Init(StreamId),
    Data(StreamId, Vec<u8>),
    Refused(StreamId),
    End(StreamId),
    Ping,
}
