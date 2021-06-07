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

mod local;
pub use magic_tunnel_lib::{StreamId, ClientId, ClientHello, ControlPacket};


pub type ActiveStreams = Arc<RwLock<HashMap<StreamId, UnboundedSender<StreamMessage>>>>;
const LOCAL_PORT: u16 = 25565;

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
        id: ClientId::generate(),
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

// TODO: improve testability, fix return value
async fn process_control_flow_message(
    mut tunnel_tx: UnboundedSender<ControlPacket>,
    payload: Vec<u8>,
    local_port: u16
) -> Result<ControlPacket, Box<dyn std::error::Error>> {
    let control_packet = ControlPacket::deserialize(&payload)?;

    match &control_packet {
        ControlPacket::Init(stream_id) => {
            info!("stream[{:?}] -> init", stream_id.to_string());

            if !ACTIVE_STREAMS.read().unwrap().contains_key(&stream_id) {
                local::setup_new_stream(
                    local_port,
                    tunnel_tx.clone(),
                    stream_id.clone(),
                )
                .await;
            } else {
                warn!("stream[{:?}] already exist at init process", stream_id.to_string());
            }
        }
        ControlPacket::Ping => {
            log::info!("got ping.");

            let _ = tunnel_tx.send(ControlPacket::Ping).await;
        }
        ControlPacket::Refused(_) => return Err("unexpected control packet".into()),
        ControlPacket::End(stream_id) => {
            // proxy server try to close control stream and local stream

            // find the stream
            let stream_id = stream_id.clone();

            info!("got end stream [{:?}]", stream_id.to_string());

            tokio::spawn(async move {
                let stream = ACTIVE_STREAMS.read().unwrap().get(&stream_id).cloned();
                if let Some(mut tx) = stream {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = tx.send(StreamMessage::Close).await.map_err(|e| {
                        error!("failed to send stream close: {:?}", e);
                    });
                    ACTIVE_STREAMS.write().unwrap().remove(&stream_id);
                }
            });
        }
        ControlPacket::Data(stream_id, data) => {
            info!(
                "stream[{:?}] -> new data: {:?}",
                stream_id.to_string(),
                data.len()
            );
            // find the right stream
            let active_stream = ACTIVE_STREAMS.read().unwrap().get(&stream_id).cloned();

            // forward data to it
            if let Some(mut tx) = active_stream {
                tx.send(StreamMessage::Data(data.clone())).await?;
                info!("forwarded to local tcp ({})", stream_id.to_string());
            } else {
                error!("got data but no stream to send it to.");
                let _ = tunnel_tx
                    .send(ControlPacket::Refused(stream_id.clone()))
                    .await?;
            }
        }
    };

    Ok(control_packet.clone())
}



#[cfg(test)]
mod process_control_flow_message {
    use super::*;

    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::io::{AsyncWriteExt, AsyncReadExt};
    use serial_test::serial;

    fn setup() {
        ACTIVE_STREAMS.write().unwrap().clear();
    }

    #[tokio::test]
    #[serial]
    async fn handle_control_packet_init() {
        setup();

        let (con_tx, con_rx) = oneshot::channel();
        let server = async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            con_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let _ = listener.accept().await.expect("No connections to accept");
        };
        tokio::spawn(server);

        let stream_id = StreamId::generate();
        let local_port = con_rx.await.expect("failed to get local port");
        let (tunnel_tx, _tunnel_rx) = unbounded::<ControlPacket>();

        // ensure active_streams has registered
        assert_eq!(0, ACTIVE_STREAMS.read().expect("failed to obtain read lock over ActiveStreams").len());
        let _ = process_control_flow_message(
            tunnel_tx,
            ControlPacket::Init(stream_id.clone()).serialize(),
            local_port,
        ).await.expect("failed to decode packet");
        assert_eq!(1, ACTIVE_STREAMS.read().expect("failed to obtain read lock over ActiveStreams").len());
    }

    #[tokio::test]
    async fn handle_control_packet_ping() {
        let (con_tx, con_rx) = oneshot::channel();
        let server = async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            con_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let _ = listener.accept().await.expect("No connections to accept");
        };
        tokio::spawn(server);

        let local_port = con_rx.await.expect("failed to get local port");
        let (tunnel_tx, mut tunnel_rx) = unbounded::<ControlPacket>();

        let _ = process_control_flow_message(
            tunnel_tx,
            ControlPacket::Ping.serialize(),
            local_port
        ).await.expect("failed to decode packet");

        // ping packet should be sent to proxy server
        assert_eq!(Some(ControlPacket::Ping), tunnel_rx.next().await);
    }


    // TODO: handle ControlPacket::Refused
    #[tokio::test]
    #[should_panic(expected = "ControlPacket::Refused may be unimplemented")]
    async fn handle_control_packet_refused() {
        let (con_tx, con_rx) = oneshot::channel();
        let server = async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            con_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let _ = listener.accept().await.expect("No connections to accept");
        };
        tokio::spawn(server);

        let stream_id = StreamId::generate();
        let local_port = con_rx.await.expect("failed to get local port");
        let (tunnel_tx, _tunnel_rx) = unbounded::<ControlPacket>();

        let _ = process_control_flow_message(
            tunnel_tx,
            ControlPacket::Refused(stream_id).serialize(),
            local_port,
        ).await.expect("ControlPacket::Refused may be unimplemented");
    }

    #[tokio::test]
    #[serial]
    async fn handle_control_packet_end() {
        setup();

        let (con_tx, con_rx) = oneshot::channel();
        let server = async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            con_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let (mut stream, _) = listener.accept().await.expect("No connections to accept");

            loop {
                stream.write_all(&b"some data".to_vec()).await.expect("failed to write packet data to local tcp socket");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        };
        tokio::spawn(server);

        let stream_id = StreamId::generate();
        let local_port = con_rx.await.expect("failed to get local port");
        let (tunnel_tx, _tunnel_rx) = unbounded::<ControlPacket>();

        // set up local connection
        let _ = process_control_flow_message(
            tunnel_tx.clone(),
            ControlPacket::Init(stream_id.clone()).serialize(),
            local_port,
        ).await.expect("failed to decode packet");
        assert_eq!(1, ACTIVE_STREAMS.read().expect("failed to obtain read lock over ActiveStreams").len());

        // terminate local connection
        let _ = process_control_flow_message(
            tunnel_tx,
            ControlPacket::End(stream_id.clone()).serialize(),
            local_port
        ).await.expect("failed to decode packet");

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(1, ACTIVE_STREAMS.read().expect("failed to obtain read lock over ActiveStreams").len());

        tokio::time::sleep(Duration::from_secs(8)).await;
        assert_eq!(0, ACTIVE_STREAMS.read().expect("failed to obtain read lock over ActiveStreams").len());
    }

    #[tokio::test]
    #[serial]
    async fn handle_control_packet_data() {
        let (con_tx, con_rx) = oneshot::channel();
        let (mut msg_tx, mut msg_rx) = unbounded();
        let server = async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            con_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let (mut stream, _) = listener.accept().await.expect("No connections to accept");
            
            let mut buf = [0; 4*1024];
            loop {
                let n = stream.read(&mut buf).await.expect("failed to read data from socket");
                if n == 0 {
                    info!("done reading from client stream");
                    return;
                }
                let data = buf[..n].to_vec();
                msg_tx.send(data).await.expect("failed to write to msg_tx");
            }
        };
        tokio::spawn(server);

        let stream_id = StreamId::generate();
        let local_port = con_rx.await.expect("failed to get local port");
        let (tunnel_tx, _tunnel_rx) = unbounded::<ControlPacket>();

        // set up local connection
        let _ = process_control_flow_message(
            tunnel_tx.clone(),
            ControlPacket::Init(stream_id.clone()).serialize(),
            local_port,
        ).await.expect("failed to decode packet");

        // receive data from server 
        let _ = process_control_flow_message(
            tunnel_tx,
            ControlPacket::Data(stream_id.clone(), b"some message 1".to_vec()).serialize(),
            local_port
        ).await.expect("failed to decode packet");


        // data should be sent to local TcpStream
        assert_eq!(Some(b"some message 1".to_vec()), msg_rx.next().await);
    }

    #[tokio::test]
    async fn handle_control_packet_data_refused() {
        let stream_id = StreamId::generate();
        let (tunnel_tx, mut tunnel_rx) = unbounded::<ControlPacket>();

        // receive data from server
        let _ = process_control_flow_message(
            tunnel_tx,
            ControlPacket::Data(stream_id.clone(), b"some message 2".to_vec()).serialize(),
            LOCAL_PORT,
        ).await.expect("failed to decode packet");

        // connection refused should be sent to proxy server
        assert_eq!(Some(ControlPacket::Refused(stream_id.clone())), tunnel_rx.next().await);
    }
}