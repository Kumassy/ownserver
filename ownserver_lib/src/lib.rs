use std::io;

use bytes::BytesMut;
use serde::{Deserialize, Serialize};
use tokio_util::codec::{Encoder, Decoder};
use uuid::Uuid;

pub const CLIENT_HELLO_VERSION: u16 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(transparent)]
pub struct StreamId(Uuid);

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stream_{}", self.0)
    }
}

impl StreamId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(transparent)]
pub struct ClientId(Uuid);

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "client_{}", self.0)
    }
}

impl ClientId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum Payload {
    Other = 0,
    UDP = 65535,
    Http = 80,
    Minecraft = 25565,
    Factorio = 34197,
}

impl std::fmt::Display for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
         match self {
            Payload::Other => write!(f, "tcp"),
            Payload::UDP => write!(f, "udp"),
            Payload::Http => write!(f, "http"),
            Payload::Minecraft => write!(f, "tcp+minecraft"),
            Payload::Factorio => write!(f, "udp+factorio"),
        }
    }
}


#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientHello {
    pub version: u16,
    pub token: String,
    pub payload: Payload,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(transparent)]
pub struct EndpointId(Uuid);
impl std::fmt::Display for EndpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "endpoint_{}", self.0)
    }
}
impl EndpointId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    TCP = 6,
    UDP = 17,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
         match self {
            Protocol::TCP => write!(f, "tcp"),
            Protocol::UDP => write!(f, "udp"),
        }
    }
}


#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct EndpointClaim {
    pub protocol: Protocol,
    pub local_port: u16,
    pub remote_port: u16,
}

pub type EndpointClaims = Vec<EndpointClaim>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientHelloV2 {
    pub version: u16,
    pub token: String,
    pub endpoint_claims: EndpointClaims,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlPacket {
    Init(StreamId),
    Data(StreamId, Vec<u8>),
    UdpData(StreamId, Vec<u8>),
    Refused(StreamId),
    End(StreamId),
    Ping,
}

impl std::fmt::Display for ControlPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
         match self {
            ControlPacket::Init(sid) => write!(f, "ControlPacket::Init(sid={})", sid),
            ControlPacket::Data(sid, data) => write!(f, "ControlPacket::Data(sid={}, data_len={})", sid, data.len()),
            ControlPacket::Refused(sid) => write!(f, "ControlPacket::Refused(sid={})", sid),
            ControlPacket::End(sid) => write!(f, "ControlPacket::End(sid={})", sid),
            ControlPacket::Ping => write!(f, "ControlPacket::Ping"),
            ControlPacket::UdpData(sid, data) => write!(f, "ControlPacket::UdpData(sid={}, data_len={})", sid, data.len()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlPacketV2 {
    Init(StreamId, EndpointId),
    Data(StreamId, Vec<u8>),
    Refused(StreamId),
    End(StreamId),
    Ping,
}

impl std::fmt::Display for ControlPacketV2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
         match self {
            ControlPacketV2::Init(sid, eid) => write!(f, "ControlPacket::Init(sid={}, eid={})", sid, eid),
            ControlPacketV2::Data(sid, data) => write!(f, "ControlPacket::Data(sid={}, data_len={})", sid,  data.len()),
            ControlPacketV2::Refused(sid)  => write!(f, "ControlPacket::Refused(sid={})", sid),
            ControlPacketV2::End(sid) => write!(f, "ControlPacket::End(sid={})", sid),
            ControlPacketV2::Ping => write!(f, "ControlPacket::Ping"),
        }
    }
}


#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ServerHello {
    Success {
        client_id: ClientId,
        /// remote addr in fqdn: foo.bar.local:43312
        remote_addr: String,
    },
    BadRequest,
    ServiceTemporaryUnavailable,
    IllegalHost,
    InternalServerError,
    VersionMismatch,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub id: EndpointId,
    pub protocol: Protocol,
    pub local_port: u16,
    pub remote_port: u16
}

pub type Endpoints = Vec<Endpoint>;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ServerHelloV2 {
    Success {
        client_id: ClientId,
        host: String,
        endpoints: Endpoints,
    },
    BadRequest,
    ServiceTemporaryUnavailable,
    IllegalHost,
    InternalServerError,
    VersionMismatch,
}

#[cfg(test)]
mod control_packet_test {
    use bytes::BytesMut;

    use super::*;

    #[test]
    fn test_control_packet_init() -> Result<(), Box<dyn std::error::Error>> {
        let stream_id = StreamId::default();
        let expected_packet = ControlPacket::Init(stream_id);

        let mut encoded = BytesMut::new();
        ControlPacketCodec::new().encode(expected_packet, &mut encoded)?;

        let deserialized_packet = ControlPacketCodec::new().decode(&mut encoded)?.unwrap();
        assert_eq!(ControlPacket::Init(stream_id), deserialized_packet);
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct ControlPacketCodec {
}
impl ControlPacketCodec {
    pub fn new() -> Self {
        Self {}
    }
}
impl Encoder<ControlPacket> for ControlPacketCodec {
    type Error = io::Error;

    fn encode(&mut self, item: ControlPacket, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let encoded = rmp_serde::to_vec(&item).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        dst.extend_from_slice(&encoded);
        Ok(())
    }
}

impl Decoder for ControlPacketCodec {
    type Item = ControlPacket;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if !src.is_empty() {
            let decoded = rmp_serde::from_slice(src).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            Ok(Some(decoded))
        } else {
            Ok(None)
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct ControlPacketV2Codec {
}
impl ControlPacketV2Codec {
    pub fn new() -> Self {
        Self {}
    }
}
impl Encoder<ControlPacketV2> for ControlPacketV2Codec {
    type Error = io::Error;

    fn encode(&mut self, item: ControlPacketV2, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let encoded = rmp_serde::to_vec(&item).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        dst.extend_from_slice(&encoded);
        Ok(())
    }
}

impl Decoder for ControlPacketV2Codec {
    type Item = ControlPacketV2;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if !src.is_empty() {
            let decoded = rmp_serde::from_slice(src).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            Ok(Some(decoded))
        } else {
            Ok(None)
        }
    }
}