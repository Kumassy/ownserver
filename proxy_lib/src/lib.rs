use serde::{Deserialize, Serialize};
use rand::prelude::*;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct StreamId(pub [u8; 8]);

impl StreamId {
    pub fn generate() -> Self {
        let mut id = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut id);
        StreamId(id)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPacket {
    Init(StreamId),
    Data(StreamId, Vec<u8>),
    Refused(StreamId),
    End(StreamId),
    Ping,
}

const EMPTY_STREAM: StreamId = StreamId([0xF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

impl ControlPacket {
    pub fn serialize(self) -> Vec<u8> {
        match self {
            ControlPacket::Init(sid) => [vec![0x01], sid.0.to_vec()].concat(),
            ControlPacket::Data(sid, data) => [vec![0x02], sid.0.to_vec(), data].concat(),
            ControlPacket::Refused(sid) => [vec![0x03], sid.0.to_vec()].concat(),
            ControlPacket::End(sid) => [vec![0x04], sid.0.to_vec()].concat(),
            ControlPacket::Ping => {
                [vec![0x05], EMPTY_STREAM.0.to_vec()].concat()
            }
        }
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        if data.len() < 9 {
            return Err("invalid DataPacket, missing stream id".into());
        }

        let mut stream_id = [0u8; 8];
        stream_id.clone_from_slice(&data[1..9]);
        let stream_id = StreamId(stream_id);

        let packet = match data[0] {
            0x01 => ControlPacket::Init(stream_id),
            0x02 => ControlPacket::Data(stream_id, data[9..].to_vec()),
            0x03 => ControlPacket::Refused(stream_id),
            0x04 => ControlPacket::End(stream_id),
            0x05 => {
                ControlPacket::Ping
            }
            _ => return Err("invalid control byte in DataPacket".into()),
        };

        Ok(packet)
    }
}

#[cfg(test)]
mod try_client_handshake_test {
    use super::*;

    #[test]
    fn test_control_packet_init() -> Result<(), Box<dyn std::error::Error>> {
        let stream_id = EMPTY_STREAM;
        let expected_packet = ControlPacket::Init(stream_id.clone());
        let deserialized_packet = ControlPacket::deserialize(&expected_packet.serialize())?;

        assert_eq!(ControlPacket::Init(EMPTY_STREAM), deserialized_packet);
        Ok(())
    }

    #[test]
    fn test_control_packet_data() -> Result<(), Box<dyn std::error::Error>> {
        let stream_id = EMPTY_STREAM;
        let expected_packet = ControlPacket::Data(stream_id.clone(), b"some data".to_vec());
        let deserialized_packet = ControlPacket::deserialize(&expected_packet.serialize())?;

        assert_eq!(ControlPacket::Data(EMPTY_STREAM, b"some data".to_vec()), deserialized_packet);
        Ok(())
    }

    #[test]
    fn test_control_packet_refused() -> Result<(), Box<dyn std::error::Error>> {
        let stream_id = EMPTY_STREAM;
        let expected_packet = ControlPacket::Refused(stream_id.clone());
        let deserialized_packet = ControlPacket::deserialize(&expected_packet.serialize())?;

        assert_eq!(ControlPacket::Refused(EMPTY_STREAM), deserialized_packet);
        Ok(())
    }

    #[test]
    fn test_control_packet_end() -> Result<(), Box<dyn std::error::Error>> {
        let stream_id = EMPTY_STREAM;
        let expected_packet = ControlPacket::End(stream_id.clone());
        let deserialized_packet = ControlPacket::deserialize(&expected_packet.serialize())?;

        assert_eq!(ControlPacket::End(EMPTY_STREAM), deserialized_packet);
        Ok(())
    }

    #[test]
    fn test_control_packet_ping() -> Result<(), Box<dyn std::error::Error>> {
        let expected_packet = ControlPacket::Ping;
        let deserialized_packet = ControlPacket::deserialize(&expected_packet.serialize())?;

        assert_eq!(ControlPacket::Ping, deserialized_packet);
        Ok(())
    }
}