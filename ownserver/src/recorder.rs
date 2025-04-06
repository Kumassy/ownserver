use std::sync::OnceLock;

use log::{info, warn};

use crate::proxy_client::ClientInfo;


static RECORDER: OnceLock<&dyn EventRecorder> = OnceLock::new();

#[derive(Debug)]
pub enum Event {
    UpdateClientInfo(ClientInfo),
}

pub trait EventRecorder: Sync + Send {
    fn log(&self, event: Event);
}

pub struct StdoutRecorder {}

impl EventRecorder for StdoutRecorder {
    fn log(&self, event: Event) {
        match event {
            Event::UpdateClientInfo(client_info) => {
                info!(
                    "cid={} got client_info from server: {:?}",
                    client_info.client_id, client_info
                );
                println!("Your Client ID: {}", client_info.client_id);
                println!("Endpoint Info:");
                for endpoint in client_info.endpoints.iter() {
                    let message = format!("{}://localhost:{} <--> {}://{}:{}", endpoint.protocol, endpoint.local_port, endpoint.protocol, client_info.host, endpoint.remote_port);
                    println!("+{}+", "-".repeat(message.len() + 2));
                    println!("| {} |", message);
                    println!("+{}+", "-".repeat(message.len() + 2));
                }
            },
        }
    }
}

pub fn recorder() -> &'static OnceLock<&'static dyn EventRecorder> {
    &RECORDER
}

pub fn init_recorder(recorder: &'static dyn EventRecorder) -> Result<(), &'static dyn EventRecorder> {
    RECORDER.set(recorder)
}

pub fn init_stdout_event_recorder() -> Result<(), &'static dyn EventRecorder> {
    let recorder = Box::leak(Box::new(StdoutRecorder {}));
    init_recorder(recorder)
}

pub fn record_client_info(client_info: ClientInfo) {
    if let Some(recorder) = RECORDER.get() {
        recorder.log(Event::UpdateClientInfo(client_info));
    } else {
        warn!("EventRecorder is not set, trying to record: {:?}", client_info);
    }
}