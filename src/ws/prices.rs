use tokio_tungstenite::connect_async;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type PriceCache = Arc<RwLock<std::collections::HashMap<String, f64>>>;

pub async fn start_ws_prices(cache: PriceCache) {
    let url = "wss://clob-ws.polymarket.com";
    let (ws, _) = connect_async(url).await.expect("WS failed");

    let (_, mut read) = ws.split();

    while let Some(msg) = read.next().await {
        if let Ok(msg) = msg {
            // parse JSON → update cache
        }
    }
}
