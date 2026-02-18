/// monitor/mod.rs — WebSocket-driven market monitor
///
/// BEFORE: REST poll every 1 second (check_interval_ms sleep)
/// AFTER:  Reacts to WS book updates in < 1 ms via tokio broadcast channel
///
/// Architecture:
///   WS feed (ws/mod.rs) ──book update──► broadcast channel
///                                              │
///                                    WsMarketMonitor::start()
///                                              │ reads from PriceCache
///                                              ▼
///                                     on_snapshot callback
///                                              │
///                                     ArbitrageDetector
use crate::cache::PriceCache;
use crate::domain::*;
use crate::ws::BookUpdate;
use anyhow::Result;
use log::{debug, info, warn};
use rust_decimal::Decimal;
use std::env;
use std::sync::Arc;
use tokio::sync::broadcast;

// ─── MarketSnapshot (unchanged shape — strategy needs no changes) ─────────────

#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub eth_market: MarketData,
    pub btc_market: MarketData,
    pub timestamp: std::time::Instant,
}

// ─── Token-to-market index ────────────────────────────────────────────────────

/// The 4 tokens we watch, plus which market+side each belongs to.
#[derive(Clone)]
pub struct WatchedTokens {
    pub eth_up_id: String,
    pub eth_down_id: String,
    pub btc_up_id: String,
    pub btc_down_id: String,
    pub eth_condition_id: String,
    pub btc_condition_id: String,
    pub eth_name: String,
    pub btc_name: String,
}

impl WatchedTokens {
    /// Returns all 4 token IDs as a Vec (for WS subscription)
    pub fn all_ids(&self) -> Vec<String> {
        vec![
            self.eth_up_id.clone(),
            self.eth_down_id.clone(),
            self.btc_up_id.clone(),
            self.btc_down_id.clone(),
        ]
    }
}

// ─── WsMarketMonitor ─────────────────────────────────────────────────────────

pub struct WsMarketMonitor {
    cache: PriceCache,
    tokens: WatchedTokens,
    ws_rx: broadcast::Receiver<BookUpdate>,
}

impl WsMarketMonitor {
    pub fn new(
        cache: PriceCache,
        tokens: WatchedTokens,
        ws_rx: broadcast::Receiver<BookUpdate>,
    ) -> Self {
        Self {
            cache,
            tokens,
            ws_rx,
        }
    }

    /// Event-driven monitoring loop.
    ///
    /// Blocks until `ws_rx` closes (e.g. 15m rollover abort).  
    /// Calls `on_snapshot` synchronously on each book event — no sleep.
    pub async fn start<F, Fut>(&mut self, on_snapshot: F)
    where
        F: Fn(MarketSnapshot) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        info!("🎬 WsMarketMonitor starting (event-driven, no polling)");

        let (signal_tx, mut signal_rx) = tokio::sync::mpsc::channel::<MarketSnapshot>(1);
        let on_snapshot = Arc::new(on_snapshot);
        tokio::spawn({
            let on_snapshot = on_snapshot.clone();
            async move {
                while let Some(snapshot) = signal_rx.recv().await {
                    on_snapshot(snapshot).await;
                }
            }
        });

        loop {
            match self.ws_rx.recv().await {
                Ok(update) => {
                    // Only process updates for tokens we actually watch
                    if !self.tokens.all_ids().contains(&update.token_id) {
                        continue;
                    }

                    debug!(
                        "⚡ WS trigger: {} ({})",
                        &update.token_id[..16.min(update.token_id.len())],
                        update.event_type
                    );

                    let mut skipped = 0usize;
                    while let Ok(next_update) = self.ws_rx.try_recv() {
                        if self.tokens.all_ids().contains(&next_update.token_id) {
                            skipped += 1;
                        }
                    }
                    if skipped > 0 {
                        debug!(
                            "⏩ Drained {} pending WS updates before snapshot build",
                            skipped
                        );
                    }

                    match self.build_snapshot().await {
                        Ok(snapshot) => {
                            if signal_tx.try_send(snapshot).is_err() {
                                debug!("⏭️ Snapshot dropped: executor still busy");
                            }
                        }
                        Err(e) => {
                            // Cache miss — book not yet received for all 4 tokens
                            debug!("📊 Snapshot incomplete (waiting for WS data): {}", e);
                        }
                    }
                }

                // Receiver lagged (dropped messages) — just continue
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("⚠️  WS receiver lagged by {} messages — continuing", n);
                }

                // Channel closed — WS task stopped (e.g. 15m rollover)
                Err(broadcast::error::RecvError::Closed) => {
                    info!("🛑 WS broadcast channel closed — monitor stopping");
                    break;
                }
            }
        }
    }

    /// Build a MarketSnapshot from the current PriceCache state.
    /// Returns Err if any of the 4 tokens don't have data yet.
    async fn build_snapshot(&self) -> Result<MarketSnapshot> {
        let required_shares = env::var("MIN_SHARES")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|f| Decimal::try_from(f).unwrap_or(Decimal::from(5)))
            .unwrap_or(Decimal::from(5));

        let eth_up = self
            .token_price(&self.tokens.eth_up_id, required_shares)
            .await?;
        let eth_down = self
            .token_price(&self.tokens.eth_down_id, required_shares)
            .await?;
        let btc_up = self
            .token_price(&self.tokens.btc_up_id, required_shares)
            .await?;
        let btc_down = self
            .token_price(&self.tokens.btc_down_id, required_shares)
            .await?;

        Ok(MarketSnapshot {
            eth_market: MarketData {
                condition_id: self.tokens.eth_condition_id.clone(),
                market_name: self.tokens.eth_name.clone(),
                up_token: Some(eth_up),
                down_token: Some(eth_down),
            },
            btc_market: MarketData {
                condition_id: self.tokens.btc_condition_id.clone(),
                market_name: self.tokens.btc_name.clone(),
                up_token: Some(btc_up),
                down_token: Some(btc_down),
            },
            timestamp: std::time::Instant::now(),
        })
    }

    async fn token_price(&self, token_id: &str, min_size: Decimal) -> Result<TokenPrice> {
        let book = self.cache.get(token_id).await.ok_or_else(|| {
            anyhow::anyhow!(
                "no WS data yet for token {}",
                &token_id[..16.min(token_id.len())]
            )
        })?;

        // Best bid: highest price in bids
        let best_bid = book.bids.first().map(|(p, _)| *p);
        let best_bid_size = book.bids.first().map(|(_, s)| *s);

        // Best ask with min liquidity, fallback to raw best ask
        let selected_ask = book
            .asks
            .iter()
            .filter(|(_, s)| *s >= min_size)
            .min_by(|(a, _), (b, _)| a.cmp(b))
            .or_else(|| book.asks.first());

        let best_ask = selected_ask.map(|(p, _)| *p);
        let best_ask_size = selected_ask.map(|(_, s)| *s);

        Ok(TokenPrice {
            token_id: token_id.to_string(),
            bid: best_bid,
            ask: best_ask,
            bid_size: best_bid_size,
            ask_size: best_ask_size,
        })
    }
}

// ─── Keep old REST MarketMonitor for fallback / diagnostics ──────────────────
// (The REST-based MarketMonitor from the original code is preserved below
//  so that the existing `discover_market` helpers still compile.
//  WsMarketMonitor is what main.rs uses in production.)

use crate::client::PolymarketClient;
use crate::execution::orderbook::fetch_orderbook;
use tokio::time::{sleep, Duration};
use tokio::try_join;

pub struct MarketMonitor {
    api: Arc<PolymarketClient>,
    left_market: Market,
    right_market: Market,
    left_name: String,
    right_name: String,
    check_interval: Duration,
}

impl MarketMonitor {
    pub fn new(
        api: Arc<PolymarketClient>,
        left_market: Market,
        right_market: Market,
        left_name: String,
        right_name: String,
        check_interval_ms: u64,
    ) -> Self {
        Self {
            api,
            left_market,
            right_market,
            left_name,
            right_name,
            check_interval: Duration::from_millis(check_interval_ms),
        }
    }

    /// Original REST polling monitor (kept for fallback / diagnostics).
    pub async fn start_monitoring<F, Fut>(&self, on_snapshot: F)
    where
        F: Fn(MarketSnapshot) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        info!(
            "🎬 REST monitor starting (polling every {:?})",
            self.check_interval
        );

        loop {
            match self.fetch_snapshot().await {
                Ok(snapshot) => on_snapshot(snapshot).await,
                Err(e) => warn!("📊 REST snapshot error: {}", e),
            }
            sleep(self.check_interval).await;
        }
    }

    async fn fetch_snapshot(&self) -> Result<MarketSnapshot> {
        let (eth_market, btc_market) = try_join!(
            self.build_market(&self.left_name, &self.left_market),
            self.build_market(&self.right_name, &self.right_market),
        )?;

        Ok(MarketSnapshot {
            eth_market,
            btc_market,
            timestamp: std::time::Instant::now(),
        })
    }

    async fn build_market(&self, name: &str, market: &Market) -> Result<MarketData> {
        let token_ids_str = market
            .clob_token_ids
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("{} market missing clob_token_ids", name))?;

        let token_ids: Vec<String> = serde_json::from_str(token_ids_str)
            .map_err(|e| anyhow::anyhow!("Failed to parse {} token IDs: {}", name, e))?;

        if token_ids.len() < 2 {
            return Err(anyhow::anyhow!("{} market has less than 2 tokens", name));
        }

        let up_token_id = &token_ids[0];
        let down_token_id = &token_ids[1];

        let (
            (up_bid, up_ask, up_bid_size, up_ask_size),
            (down_bid, down_ask, down_bid_size, down_ask_size),
        ) = tokio::join!(
            self.fetch_token_top(name, "UP", up_token_id),
            self.fetch_token_top(name, "DOWN", down_token_id),
        );

        Ok(MarketData {
            condition_id: market.condition_id.clone(),
            market_name: name.to_string(),
            up_token: Some(TokenPrice {
                token_id: up_token_id.clone(),
                bid: up_bid,
                ask: up_ask,
                bid_size: up_bid_size,
                ask_size: up_ask_size,
            }),
            down_token: Some(TokenPrice {
                token_id: down_token_id.clone(),
                bid: down_bid,
                ask: down_ask,
                bid_size: down_bid_size,
                ask_size: down_ask_size,
            }),
        })
    }

    async fn fetch_token_top(
        &self,
        market_name: &str,
        outcome_name: &str,
        token_id: &str,
    ) -> (
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
    ) {
        match fetch_orderbook(&self.api, token_id).await {
            Ok(book) => {
                let required_shares = env::var("MIN_SHARES")
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(5.0);

                let best_bid = book
                    .best_bid()
                    .map(|(p, _)| Decimal::try_from(p).unwrap_or(Decimal::ZERO));
                let best_bid_size = book
                    .best_bid()
                    .map(|(_, s)| Decimal::try_from(s).unwrap_or(Decimal::ZERO));

                let selected_ask = book
                    .cheapest_ask_with_min_size(required_shares)
                    .or_else(|| book.best_ask());
                let best_ask =
                    selected_ask.map(|(p, _)| Decimal::try_from(p).unwrap_or(Decimal::ZERO));
                let best_ask_size =
                    selected_ask.map(|(_, s)| Decimal::try_from(s).unwrap_or(Decimal::ZERO));

                (best_bid, best_ask, best_bid_size, best_ask_size)
            }
            Err(e) => {
                warn!(
                    "⚠️  Failed to fetch {} {} prices: {}",
                    market_name, outcome_name, e
                );
                (None, None, None, None)
            }
        }
    }
}
