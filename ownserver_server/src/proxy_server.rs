pub use ownserver_lib::{ClientId, StreamId};
use std::sync::Arc;
use tokio::task::JoinSet;
use once_cell::sync::OnceCell;

use crate::{control_server_v2, Store};
use crate::Config;

#[tracing::instrument(skip(config, store))]
pub async fn run(
    config: &'static OnceCell<Config>,
    store: Arc<Store>,
) -> JoinSet<()> {
    tracing::info!("starting server!");

    let control_port = config.get().expect("failed to read config").control_port;

    let set = control_server_v2::spawn(
        config,
        store,
        ([0, 0, 0, 0], control_port));
    tracing::info!("started tunnelto server on 0.0.0.0:{}", control_port);
    set
}
