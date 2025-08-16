use ownserver_lib::{StreamId, ClientId, ControlPacketV2};

use crate::{SendToClientError, SendToRemoteError};

use super::{tcp::RemoteTcp, udp::RemoteUdp};


#[derive(Debug)]
pub enum RemoteStream {
    RemoteTcp(RemoteTcp),
    RemoteUdp(RemoteUdp),
}

impl RemoteStream {
    pub async fn send_to_remote(&mut self, stream_id: StreamId, data: &[u8]) -> Result<(), SendToRemoteError> {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.send_to_remote(stream_id, data).await
            }
            RemoteStream::RemoteUdp(udp) => {
                udp.send_to_remote(stream_id, data).await
            }
        }
    }

    pub async fn send_to_client(&self, packet: ControlPacketV2) -> Result<(), SendToClientError> {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.send_to_client(packet).await
            }
            RemoteStream::RemoteUdp(udp) => {
                udp.send_to_client(packet).await
            }
        }
    }


    pub fn disable(&mut self) {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.disable();
            }
            RemoteStream::RemoteUdp(udp) => {
                udp.disable();
            }
        }
    }

    pub fn disabled(&self) -> bool {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.disabled()
            }
            RemoteStream::RemoteUdp(udp) => {
                udp.disabled()
            }
        }
    }

    pub fn stream_id(&self) -> StreamId {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.stream_id
            }
            RemoteStream::RemoteUdp(udp) => {
                udp.stream_id
            }
        }
    }

    pub fn client_id(&self) -> ClientId {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.client_id
            }
            RemoteStream::RemoteUdp(udp) => {
                udp.client_id
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamMessage {
    Data(Vec<u8>),
    TunnelRefused,
    NoClientTunnel,
}