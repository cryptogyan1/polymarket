/// ws/mod.rs — Polymarket CLOB WebSocket feed
///
/// Corrected against official docs at docs.polymarket.com
///
/// ── Key facts from docs ────────────────────────────────────────────────────
///
/// SUBSCRIPTION (sent on connect):
///   { "assets_ids": [...token_ids...], "type": "market" }
///   NOTE: field is "assets_ids" (plural), type value is lowercase "market"
///
/// DYNAMIC SUBSCRIBE (add tokens after connect):
///   { "assets_ids": [...], "operation": "subscribe" }
///
/// PING (text frame, NOT WebSocket protocol ping):
///   "PING"   ← raw string, not JSON
///
/// RECEIVED MESSAGES all use "event_type" field (NOT "type"):
///   { "event_type": "book",         "asset_id": ..., "bids": [...], "asks": [...] }
///   { "event_type": "price_change", "market": ...,  "price_changes": [...] }
///   { "event_type": "last_trade_price", "asset_id": ..., "price": ... }
///   { "event_type": "best_bid_ask", "asset_id": ..., "best_bid": ..., "best_ask": ... }
///   { "event_type": "tick_size_change", ... }
///
/// price_change BREAKING CHANGE (Sept 15 2025):
///   OLD: { "asset_id": "...", "changes": [{price, size}] }
///   NEW: { "price_changes": [{ "asset_id": "...", "price", "size", "side": "BUY"|"SELL",
///                              "best_bid", "best_ask", "hash" }] }
///   The "side" field tells us exactly which side of the book changed — no guessing.
///   We use best_bid/best_ask directly from the message for speed.
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

// ─── Public trigger ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BookUpdate {
    pub token_id: String,
    pub event_type: String,
    /// best_bid/best_ask when available directly in the message (price_change / best_bid_ask)
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
}

// ─── Deserialization structs ──────────────────────────────────────────────────

/// Level in a `book` snapshot
#[derive(Debug, Deserialize)]
struct WsLevel {
    price: String,
    size: String,
}

/// Full orderbook snapshot (event_type = "book")
#[derive(Debug, Deserialize)]
struct WsBookEvent {
    asset_id: String,
    #[serde(default)]
    bids: Vec<WsLevel>,
    #[serde(default)]
    asks: Vec<WsLevel>,
}

/// Single price-level change inside a price_change event (new format post-Sept-2025)
#[derive(Debug, Deserialize)]
struct WsPriceChange {
    asset_id: String,
    price: String,
    size: String,
    side: String, // "BUY" or "SELL"
    #[serde(default)]
    best_bid: Option<String>,
    #[serde(default)]
    best_ask: Option<String>,
}

/// price_change envelope (event_type = "price_change")
/// Supports both old format (changes[]) and new format (price_changes[])
#[derive(Debug, Deserialize)]
struct WsPriceChangeEvent {
    // new format (post-Sept-2025)
    #[serde(default)]
    price_changes: Vec<WsPriceChange>,
}

/// best_bid_ask message (event_type = "best_bid_ask", requires custom_feature_enabled=true)
#[derive(Debug, Deserialize)]
struct WsBestBidAsk {
    asset_id: String,
    best_bid: String,
    best_ask: String,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Spawn the WS feed in the background.
/// Returns a broadcast Receiver that fires on every orderbook update.
///
/// * `ws_url`    – e.g. "wss://ws-subscriptions-clob.polymarket.com/ws/market"
/// * `token_ids` – 4 token IDs from CLOB API (77-digit decimal strings)
/// * `cache`     – shared PriceCache written on every update
pub fn spawn_ws_feed(
    ws_url: String,
    token_ids: Vec<String>,
    cache: PriceCache,
) -> broadcast::Receiver<BookUpdate> {
    let (tx, rx) = broadcast::channel::<BookUpdate>(2048);

    tokio::spawn(async move {
        loop {
            match connect_and_stream(&ws_url, &token_ids, &cache, &tx).await {
                Ok(_) => warn!("WS stream ended cleanly — reconnecting in 100 ms"),
                Err(e) => warn!("WS error: {} — reconnecting in 100 ms", e),
            }
            sleep(Duration::from_millis(100)).await;
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

    // ── Subscription ────────────────────────────────────────────────────────
    // Correct format per official docs:
    //   { "assets_ids": [...], "type": "market" }
    //
    // Optionally add "custom_feature_enabled": true to also receive
    //   best_bid_ask and new_market/market_resolved messages.
    let sub = json!({
        "assets_ids": token_ids,
        "type": "market",
        "custom_feature_enabled": true
    });
    write.send(Message::Text(sub.to_string())).await?;

    info!("📡 WS subscribed to {} tokens", token_ids.len());

    // ── Heartbeat ────────────────────────────────────────────────────────────
    // Docs Python example sends the literal string "PING" (not a WS Ping frame)
    let mut hb = interval(Duration::from_secs(10));
    hb.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = hb.tick() => {
                // Send "PING" as a text frame — NOT a WebSocket protocol Ping
                if let Err(e) = write.send(Message::Text("PING".to_string())).await {
                    return Err(anyhow::anyhow!("heartbeat failed: {}", e));
                }
                debug!("💓 PING sent");
            }

            msg = read.next() => {
                let msg = msg.ok_or_else(|| anyhow::anyhow!("WS stream closed"))??;
                match msg {
                    Message::Text(txt) => {
                        handle_message(&txt, cache, tx).await;
                    }
                    Message::Ping(data) => {
                        // Respond to server-initiated WS pings
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Message::Close(_) => {
                        return Err(anyhow::anyhow!("WS closed by server"));
                    }
                    _ => {}
                }
            }
        }
    }
}

// ─── Message handler ──────────────────────────────────────────────────────────

async fn handle_message(txt: &str, cache: &PriceCache, tx: &broadcast::Sender<BookUpdate>) {
    // Server echoes "PONG" in response to our "PING"
    if txt == "PONG" {
        debug!("💓 PONG received");
        return;
    }

    let v: Value = match serde_json::from_str(txt) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "WS JSON parse error: {} | raw= {}",
                e,
                &txt[..txt.len().min(120)]
            );
            return;
        }
    };

    // ══════════════════════════════════════════════════════════════════════════
    // ROOT CAUSE FIX: Polymarket sends the initial book snapshots as a JSON ARRAY:
    //   [{"event_type":"book","asset_id":...,"bids":[...],"asks":[...]}, ...]
    //
    // v.get("event_type") on an array returns None → early return → snapshot
    // silently dropped → cache empty forever → every price_change also skipped.
    //
    // Fix: detect array, iterate each element through handle_value().
    // ══════════════════════════════════════════════════════════════════════════
    if let Some(arr) = v.as_array() {
        for item in arr {
            handle_value(item, cache, tx).await;
        }
        return;
    }

    handle_value(&v, cache, tx).await;
}

// Processes a single JSON object with an "event_type" field.
// Called both directly (plain object messages) and from the array unwrapper above.
async fn handle_value(v: &Value, cache: &PriceCache, tx: &broadcast::Sender<BookUpdate>) {
    // ══════════════════════════════════════════════════════════════════════════
    // MARKET CHANNEL messages use "event_type" (NOT "type").
    // "type" is only used in the subscription message we SEND.
    // ══════════════════════════════════════════════════════════════════════════
    let event_type = match v.get("event_type").and_then(Value::as_str) {
        Some(t) => t,
        None => {
            debug!("WS msg no event_type (skipped)");
            return;
        }
    };

    match event_type {
        // ── Full orderbook snapshot ────────────────────────────────────────
        // Emitted: on first subscribe + after each trade that changes the book
        "book" => {
            let ev: WsBookEvent = match serde_json::from_value(v.clone()) {
                Ok(e) => e,
                Err(e) => {
                    warn!("Failed to parse 'book' event: {}", e);
                    return;
                }
            };

            let mut bids = parse_levels(&ev.bids);
            let mut asks = parse_levels(&ev.asks);
            bids.sort_by(|a, b| b.0.cmp(&a.0)); // descending (best first)
            asks.sort_by(|a, b| a.0.cmp(&b.0)); // ascending  (best first)

            let best_bid = bids.first().map(|(p, _)| *p);
            let best_ask = asks.first().map(|(p, _)| *p);

            info!(
                "📚 book  {}…  bid={} ask={}  ({} levels)",
                &ev.asset_id[..ev.asset_id.len().min(20)],
                best_bid.map(|p| format!("{:.4}", p)).unwrap_or("—".into()),
                best_ask.map(|p| format!("{:.4}", p)).unwrap_or("—".into()),
                bids.len() + asks.len()
            );

            cache.update(&ev.asset_id, bids, asks).await;

            let _ = tx.send(BookUpdate {
                token_id: ev.asset_id,
                event_type: "book".to_string(),
                best_bid,
                best_ask,
            });
        }

        // ── Incremental price-level changes ────────────────────────────────
        // New format (post-Sept-2025 breaking change):
        //   price_changes[].side = "BUY" | "SELL"  — tells us exactly which side
        //   price_changes[].best_bid / best_ask     — available directly, no parsing needed
        "price_change" => {
            let ev: WsPriceChangeEvent = match serde_json::from_value(v.clone()) {
                Ok(e) => e,
                Err(e) => {
                    warn!("Failed to parse price_change: {}", e);
                    return;
                }
            };

            // Group changes by asset_id (one event can contain changes for multiple tokens)
            // e.g. YES side of BTC market + NO side of BTC market in one message
            let mut seen_assets: std::collections::HashMap<
                String,
                (Option<Decimal>, Option<Decimal>),
            > = std::collections::HashMap::new();

            for change in &ev.price_changes {
                let price = match Decimal::from_str(&change.price) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let size = match Decimal::from_str(&change.size) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                let best_bid = change
                    .best_bid
                    .as_deref()
                    .and_then(|s| Decimal::from_str(s).ok());
                let best_ask = change
                    .best_ask
                    .as_deref()
                    .and_then(|s| Decimal::from_str(s).ok());

                seen_assets.insert(change.asset_id.clone(), (best_bid, best_ask));

                if let Some(book) = cache.get(&change.asset_id).await {
                    let mut bids = book.bids.clone();
                    let mut asks = book.asks.clone();

                    match change.side.to_uppercase().as_str() {
                        "BUY" => {
                            upsert_level(&mut bids, price, size);
                            bids.sort_by(|a, b| b.0.cmp(&a.0));
                        }
                        "SELL" => {
                            upsert_level(&mut asks, price, size);
                            asks.sort_by(|a, b| a.0.cmp(&b.0));
                        }
                        _ => {
                            // Unknown side — apply to whichever existing side matches
                            apply_level_unknown_side(&mut bids, &mut asks, price, size);
                        }
                    }
                    cache.update(&change.asset_id, bids, asks).await;
                }
                // If no existing book yet, wait for the next full "book" snapshot
            }

            for (asset_id, (best_bid, best_ask)) in seen_assets {
                debug!(
                    "📈 price_change {}…  bid={} ask={}",
                    &asset_id[..asset_id.len().min(20)],
                    best_bid.map(|p| format!("{:.4}", p)).unwrap_or("—".into()),
                    best_ask.map(|p| format!("{:.4}", p)).unwrap_or("—".into()),
                );
                let _ = tx.send(BookUpdate {
                    token_id: asset_id,
                    event_type: "price_change".to_string(),
                    best_bid,
                    best_ask,
                });
            }
        }

        // ── Best bid/ask shortcut (custom_feature_enabled=true) ────────────
        // Fastest possible arb detection — no book parsing needed at all.
        // We get best_bid and best_ask directly in the message.
        "best_bid_ask" => {
            let ev: WsBestBidAsk = match serde_json::from_value(v.clone()) {
                Ok(e) => e,
                Err(_) => return,
            };

            let best_bid = Decimal::from_str(&ev.best_bid).ok();
            let best_ask = Decimal::from_str(&ev.best_ask).ok();

            if let (Some(bid), Some(ask)) = (best_bid, best_ask) {
                cache
                    .update(
                        &ev.asset_id,
                        vec![(bid, Decimal::ONE)],
                        vec![(ask, Decimal::ONE)],
                    )
                    .await;
            }

            debug!(
                "⚡ best_bid_ask {}…  bid={} ask={}",
                &ev.asset_id[..ev.asset_id.len().min(20)],
                ev.best_bid,
                ev.best_ask
            );

            let _ = tx.send(BookUpdate {
                token_id: ev.asset_id,
                event_type: "best_bid_ask".to_string(),
                best_bid,
                best_ask,
            });
        }

        // ── Last trade price ───────────────────────────────────────────────
        // Emitted when a maker+taker order is matched — confirms a real trade happened
        "last_trade_price" => {
            let asset_id = v.get("asset_id").and_then(Value::as_str).unwrap_or("");
            let price_str = v.get("price").and_then(Value::as_str).unwrap_or("0");
            let side = v.get("side").and_then(Value::as_str).unwrap_or("?");
            let size_str = v.get("size").and_then(Value::as_str).unwrap_or("0");

            if !asset_id.is_empty() {
                debug!(
                    "💹 last_trade {}…  {} @ {} (size={})",
                    &asset_id[..asset_id.len().min(20)],
                    side,
                    price_str,
                    size_str
                );
                // Fire a trigger — arb detector will re-check the cached book
                let _ = tx.send(BookUpdate {
                    token_id: asset_id.to_string(),
                    event_type: "last_trade_price".to_string(),
                    best_bid: None,
                    best_ask: None,
                });
            }
        }

        // ── Market resolved — log winning outcome ──────────────────────────
        // Useful for triggering auto-redeem in the future
        "market_resolved" => {
            let winning = v
                .get("winning_outcome")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let winning_id = v
                .get("winning_asset_id")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let market = v.get("market").and_then(Value::as_str).unwrap_or("?");
            info!(
                "🏁 market_resolved | condition={} | winner={} | token={}…",
                &market[..market.len().min(20)],
                winning,
                &winning_id[..winning_id.len().min(20)]
            );
        }

        "new_market" => debug!("WS: new_market event (ignored)"),
        "tick_size_change" => debug!("WS: tick_size_change event (ignored)"),

        other => {
            debug!("WS event_type='{}' — ignored", other);
        }
    }
}

// ─── Level helpers ────────────────────────────────────────────────────────────

/// Insert or update a price level. Removes the level if size == 0.
fn upsert_level(levels: &mut Vec<(Decimal, Decimal)>, price: Decimal, size: Decimal) {
    if size == Decimal::ZERO {
        levels.retain(|(p, _)| *p != price);
        return;
    }
    if let Some(e) = levels.iter_mut().find(|(p, _)| *p == price) {
        e.1 = size;
    } else {
        levels.push((price, size));
    }
}

/// Fallback when side is unknown — check existing levels to determine side.
fn apply_level_unknown_side(
    bids: &mut Vec<(Decimal, Decimal)>,
    asks: &mut Vec<(Decimal, Decimal)>,
    price: Decimal,
    size: Decimal,
) {
    if size == Decimal::ZERO {
        bids.retain(|(p, _)| *p != price);
        asks.retain(|(p, _)| *p != price);
        return;
    }
    if bids.iter().any(|(p, _)| *p == price) {
        upsert_level(bids, price, size);
        return;
    }
    if asks.iter().any(|(p, _)| *p == price) {
        upsert_level(asks, price, size);
        return;
    }
    // New level — infer side from position vs current best bid
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
