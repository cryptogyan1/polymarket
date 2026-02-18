/// ws/mod.rs — WebSocket-driven market data feed
///
/// Replaces REST polling entirely. Connects to Polymarket's CLOB WebSocket,
/// subscribes to `agg_orderbook` events for the 4 tokens (BTC_UP, BTC_DOWN,
/// ETH_UP, ETH_DOWN), updates the PriceCache on every message, and fires a
/// tokio broadcast channel so the monitor can react instantly (< 1 ms).
///
/// Message format received from Polymarket:
///   { "event_type": "book",   "asset_id": "<token_id>",
///     "bids": [{"price":"0.45","size":"100"}],
///     "asks": [{"price":"0.55","size":"150"}] }
///
///   { "event_type": "price_change", "asset_id": "<token_id>",
///     "changes": [{"price":"0.45","size":"100"}] }
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use std::str::FromStr;
use tokio::sync::broadcast;
use tokio::time::{interval, sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use crate::cache::PriceCache;

// ─── Public trigger ──────────────────────────────────────────────────────────

/// Fired on the broadcast channel every time any watched token's book changes.
#[derive(Debug, Clone)]
pub struct BookUpdate {
    pub token_id: String,
    pub event_type: String,
}

// ─── Raw WS deserialization ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WsLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct WsBookEvent {
    asset_id: String,
    #[serde(default)]
    bids: Vec<WsLevel>,
    #[serde(default)]
    asks: Vec<WsLevel>,
}

#[derive(Debug, Deserialize)]
struct WsPriceChangeEvent {
    asset_id: String,
    #[serde(default)]
    changes: Vec<WsLevel>,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Spawn the WS feed in the background.
/// Returns a broadcast Receiver that fires on every orderbook update.
///
/// * `ws_url`    – e.g. "wss://ws-subscriptions-clob.polymarket.com/ws/market"
/// * `token_ids` – 4 token IDs: BTC_UP, BTC_DOWN, ETH_UP, ETH_DOWN
/// * `cache`     – shared PriceCache written on every update
pub fn spawn_ws_feed(
    ws_url: String,
    token_ids: Vec<String>,
    cache: PriceCache,
) -> broadcast::Receiver<BookUpdate> {
    let (tx, rx) = broadcast::channel::<BookUpdate>(256);

    tokio::spawn(async move {
        loop {
            match connect_and_stream(&ws_url, &token_ids, &cache, &tx).await {
                Ok(_) => warn!("WS stream ended cleanly — reconnecting in 2 s"),
                Err(e) => warn!("WS error: {} — reconnecting in 2 s", e),
            }
            sleep(Duration::from_secs(2)).await;
        }
    });

    rx
}

// ─── Connection loop ──────────────────────────────────────────────────────────

async fn connect_and_stream(
    ws_url: &str,
    token_ids: &[String],
    cache: &PriceCache,
    tx: &broadcast::Sender<BookUpdate>,
) -> Result<()> {
    info!("🔌 Connecting to CLOB WebSocket: {}", ws_url);

    let (ws, _) = connect_async(Url::parse(ws_url)?).await?;
    let (mut write, mut read) = ws.split();

    // Subscribe to agg_orderbook + price_change for all tokens
    let subscribe_msg = json!({
        "type": "subscribe",
        "channels": [{ "name": "market", "token_ids": token_ids }]
    });
    write.send(Message::Text(subscribe_msg.to_string())).await?;

    info!("📡 WS subscribed to {} tokens", token_ids.len());

    let mut hb = interval(Duration::from_secs(20));

    loop {
        tokio::select! {
            _ = hb.tick() => {
                let ping = json!({"type":"ping"});
                if let Err(e) = write.send(Message::Text(ping.to_string())).await {
                    return Err(anyhow::anyhow!("ping failed: {}", e));
                }
                debug!("💓 WS ping sent");
            }

            msg = read.next() => {
                let msg = msg.ok_or_else(|| anyhow::anyhow!("WS stream closed"))??;
                if let Message::Text(txt) = msg {
                    handle_message(&txt, cache, tx).await;
                }
            }
        }
    }
}

// ─── Message parsing ──────────────────────────────────────────────────────────

async fn handle_message(txt: &str, cache: &PriceCache, tx: &broadcast::Sender<BookUpdate>) {
    let v: Value = match serde_json::from_str(txt) {
        Ok(v) => v,
        Err(_) => return,
    };

    let event_type = match v.get("event_type").and_then(Value::as_str) {
        Some(t) => t,
        None => return, // ack / ping — skip
    };

    match event_type {
        // Full snapshot — replaces the whole book for this token
        "book" => {
            if let Ok(ev) = serde_json::from_value::<WsBookEvent>(v) {
                let mut bids = parse_levels(&ev.bids);
                let mut asks = parse_levels(&ev.asks);
                bids.sort_by(|a, b| b.0.cmp(&a.0)); // descending
                asks.sort_by(|a, b| a.0.cmp(&b.0)); // ascending

                debug!(
                    "📚 book update {} | {} bids {} asks",
                    short(&ev.asset_id),
                    bids.len(),
                    asks.len()
                );

                cache.update(&ev.asset_id, bids, asks).await;
                let _ = tx.send(BookUpdate {
                    token_id: ev.asset_id,
                    event_type: "book".to_string(),
                });
            }
        }

        // Incremental — merge individual price-level changes into existing book
        "price_change" => {
            if let Ok(ev) = serde_json::from_value::<WsPriceChangeEvent>(v) {
                if let Some(mut book) = cache.get(&ev.asset_id).await {
                    for change in &ev.changes {
                        apply_change(&mut book.bids, &mut book.asks, change);
                    }
                    book.bids.sort_by(|a, b| b.0.cmp(&a.0));
                    book.asks.sort_by(|a, b| a.0.cmp(&b.0));
                    cache.update(&ev.asset_id, book.bids, book.asks).await;
                }

                debug!(
                    "📈 price_change {} | {} changes",
                    short(&ev.asset_id),
                    ev.changes.len()
                );

                let _ = tx.send(BookUpdate {
                    token_id: ev.asset_id,
                    event_type: "price_change".to_string(),
                });
            }
        }

        other => debug!("WS event_type='{}' — ignored", other),
    }
}

/// Applies a single price-level change to the live bids/asks vectors.
fn apply_change(
    bids: &mut Vec<(Decimal, Decimal)>,
    asks: &mut Vec<(Decimal, Decimal)>,
    change: &WsLevel,
) {
    let price = match Decimal::from_str(&change.price) {
        Ok(p) => p,
        Err(_) => return,
    };
    let size = match Decimal::from_str(&change.size) {
        Ok(s) => s,
        Err(_) => return,
    };

    if size == Decimal::ZERO {
        bids.retain(|(p, _)| *p != price);
        asks.retain(|(p, _)| *p != price);
        return;
    }

    // Try bids first, then asks, then infer from position vs best bid
    if let Some(e) = bids.iter_mut().find(|(p, _)| *p == price) {
        e.1 = size;
        return;
    }
    if let Some(e) = asks.iter_mut().find(|(p, _)| *p == price) {
        e.1 = size;
        return;
    }

    // New level — side determined by price vs current best bid
    let best_bid = bids.iter().map(|(p, _)| *p).max().unwrap_or(Decimal::ZERO);
    if price <= best_bid {
        bids.push((price, size));
    } else {
        asks.push((price, size));
    }
}

fn parse_levels(levels: &[WsLevel]) -> Vec<(Decimal, Decimal)> {
    levels
        .iter()
        .filter_map(|l| {
            let p = Decimal::from_str(&l.price).ok()?;
            let s = Decimal::from_str(&l.size).ok()?;
            Some((p, s))
        })
        .collect()
}

fn short(s: &str) -> &str {
    &s[..16.min(s.len())]
}
