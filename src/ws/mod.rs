use crate::cache::PriceCache;
use crate::client::PolymarketClient;
use futures_util::{SinkExt, StreamExt};
use log::{info, warn};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{interval, sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

pub async fn start_ws(
    ws_url: String,
    cache: PriceCache,
    token_ids: Vec<String>,
    api: Arc<PolymarketClient>,
) {
    loop {
        info!("🔌 Connecting to CLOB WebSocket");
        let api_clone = api.clone();

        if let Err(e) = connect_and_stream(&ws_url, &cache, &token_ids, api_clone).await {
            warn!("⚠️ WS error: {} — reconnecting in 2s", e);
            sleep(Duration::from_secs(2)).await;
        }
    }
}

async fn connect_and_stream(
    ws_url: &str,
    cache: &PriceCache,
    token_ids: &Vec<String>,
    api: Arc<PolymarketClient>,
) -> anyhow::Result<()> {
    let (ws, _) = connect_async(Url::parse(ws_url)?).await?;
    let (mut write, mut read) = ws.split();

    // ---------- AUTH (NO SIGNATURE — READ ONLY) ----------
    let auth = json!({
        "type": "auth",
        "apiKey": api.api_key,
        "passphrase": std::env::var("POLY_API_PASSPHRASE")?,
        "timestamp": chrono::Utc::now().timestamp().to_string()
    });
    write.send(Message::Text(auth.to_string())).await?;

    // ---------- SUBSCRIBE ----------
    let sub = json!({
        "type": "subscribe",
        "channels": [{
            "name": "market",
            "token_ids": token_ids
        }]
    });
    write.send(Message::Text(sub.to_string())).await?;

    info!("📡 WS connected & subscribed");

    let mut hb = interval(Duration::from_secs(20));

    loop {
        tokio::select! {
            _ = hb.tick() => {
                let _ = write
                    .send(Message::Text(json!({"type":"ping"}).to_string()))
                    .await;
            }
            msg = read.next() => {
                let msg = msg.ok_or_else(|| anyhow::anyhow!("WS closed"))??;

                if let Message::Text(txt) = msg {
                    if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                        if let Some(token_id) = v.get("token_id").and_then(|t| t.as_str()) {
                            cache.update_from_price_ws(token_id, &v).await;
                        }
                    }
                }
            }
        }
    }
}
