use std::sync::Arc;

use futures::Future;
use warp::Filter;

use crate::{Store, proxy_client::ClientInfo};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Endpoint {
    id: String,
    local_port: u16,
    remote_addr: String,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Stream {
    id: String,
}

pub fn spawn_api(store: Arc<Store>, api_port: u16, local_port: u16, client_info: ClientInfo) -> impl Future<Output = ()> {
    let store_ = store.clone();
    let endpoints = warp::path("endpoints").map(move || {
        let endpoints = store_.get_endpoints();
        warp::reply::json(&vec![endpoints])
    });
    let streams = warp::path("streams").map(move || {
        let streams = store.list_streams()
            .iter()
            .map(|id| Stream {
                id: id.to_string(),
            })
            .collect::<Vec<Stream>>();
        warp::reply::json(
            &streams
        )
    });
    let routes = warp::get().and(
        endpoints
            .or(streams)
    );
    warp::serve(routes).run(([127, 0, 0, 1], api_port))
}
