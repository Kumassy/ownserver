use std::sync::Arc;

use futures::Future;
use warp::Filter;

use crate::Store;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Stream {
    id: String,
}

pub fn spawn_api(store: Arc<Store>, api_port: u16) -> impl Future<Output = ()> {
    let store_ = store.clone();
    let rtt = warp::path("rtt").map(move || {
        let rtt = store_.get_rtt();
        warp::reply::json(&rtt)
    });
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
            .or(rtt)
    );
    warp::serve(routes).run(([127, 0, 0, 1], api_port))
}
