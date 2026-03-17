/// main.rs — WS-driven Polymarket arbitrage bot
///
/// Flow:
///   1. Discover current 15m markets (BTC + ETH pair) via REST
///   2. Spawn WS feed subscribing to all 4 tokens
///   3. WsMarketMonitor reacts to book updates (< 1 ms)
///   4. ArbitrageDetector checks opportunity on every update
///   5. Trader executes BOTH legs in parallel (tokio::join!)
///   6. On 15m rollover: restart WS feed + monitor for new markets
use polymarket_15m_arbitrage_bot::*;

use anyhow::{Context, Result};
use clap::Parser;
use config::{Args, Config};
use futures_util::{stream, StreamExt};
use log::{info, warn};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Mutex};

use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::config::WalletConfig;
use cache::PriceCache;
use client::PolymarketClient;
use ethers::providers::{Http, Provider};
use execution::{clob_client::ClobClient, ExecutorClient, Trader};
use monitor::{WatchedTokens, WsMarketMonitor};
use strategy::ArbitrageDetector;
use wallet::allowance::verify_allowances;
use wallet::signer::WalletSigner;
use ws::spawn_ws_feed;

// ─── Pair config ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct PairConfig {
    left_name: &'static str,
    left_prefix: &'static str,
    right_name: &'static str,
    right_prefix: &'static str,
}

#[derive(Debug, Clone)]
struct SportsMarket {
    slug: String,
    condition_id: String,
    question: String,
    outcome_a_label: String,
    outcome_a_token: String,
    outcome_b_label: String,
    outcome_b_token: String,
    sports_market_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SportsMarketTypesResponse {
    #[serde(rename = "marketTypes", default)]
    market_types: Vec<String>,
}

#[derive(Debug)]
struct SportsOpportunity {
    market: SportsMarket,
    outcome_a_ask: f64,
    outcome_b_ask: f64,
    sum: f64,
    edge: f64,
}

fn parse_env_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn env_bool_any_optional(keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Ok(value) = std::env::var(key) {
            return Some(parse_env_bool(&value));
        }
    }
    None
}

fn selected_pair_config() -> Result<PairConfig> {
    let btc_eth = env_bool_any_optional(&["PAIR_BTC_ETH", "BTC_ETH", "BTC-ETH"]);
    let btc_sol = env_bool_any_optional(&["PAIR_BTC_SOL", "BTC_SOL", "BTC-SOL"]);
    let btc_xrp = env_bool_any_optional(&["PAIR_BTC_XRP", "BTC_XRP", "BTC-XRP"]);

    let any_explicit = btc_eth.is_some() || btc_sol.is_some() || btc_xrp.is_some();
    let use_btc_eth = btc_eth.unwrap_or(!any_explicit);
    let use_btc_sol = btc_sol.unwrap_or(false);
    let use_btc_xrp = btc_xrp.unwrap_or(false);

    let count = [use_btc_eth, use_btc_sol, use_btc_xrp]
        .iter()
        .filter(|&&v| v)
        .count();
    if count != 1 {
        anyhow::bail!("Exactly one pair must be enabled: PAIR_BTC_ETH, PAIR_BTC_SOL, PAIR_BTC_XRP");
    }

    if use_btc_eth {
        return Ok(PairConfig {
            left_name: "BTC",
            left_prefix: "btc",
            right_name: "ETH",
            right_prefix: "eth",
        });
    }
    if use_btc_sol {
        return Ok(PairConfig {
            left_name: "BTC",
            left_prefix: "btc",
            right_name: "SOL",
            right_prefix: "sol",
        });
    }
    Ok(PairConfig {
        left_name: "BTC",
        left_prefix: "btc",
        right_name: "XRP",
        right_prefix: "xrp",
    })
}

// ─── 15-minute period helper ──────────────────────────────────────────────────

fn current_15m_period() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    (now / 900) * 900
}

#[derive(Debug, Clone)]
struct SimulatedTrade {
    id: u64,
    opened_at: std::time::Instant,
    pair_label: String,
    shares: Decimal,
    entry_total: Decimal,
    eth_token_id: String,
    btc_token_id: String,
}

#[derive(Debug, Default, Clone)]
struct SimulationStats {
    seen_signals: u64,
    opened_trades: u64,
    closed_trades: u64,
    wins: u64,
    losses: u64,
    failed_trades: u64,
    failed_legs: u64,
    missing_data_events: u64,
    low_liquidity_skips: u64,
    gross_pnl_usd: Decimal,
    by_failure_reason: HashMap<String, u64>,
    by_pair_opened: HashMap<String, u64>,
}

struct SimulationEngine {
    stats: SimulationStats,
    open_positions: Vec<SimulatedTrade>,
    next_id: u64,
    hold_seconds: u64,
    stake_per_trade: Decimal,
}

impl SimulationEngine {
    fn new(hold_seconds: u64, stake_per_trade: Decimal) -> Self {
        Self {
            stats: SimulationStats::default(),
            open_positions: Vec::new(),
            next_id: 1,
            hold_seconds,
            stake_per_trade,
        }
    }

    fn register_opportunity(&mut self, opp: &domain::ArbitrageOpportunity) {
        self.stats.seen_signals += 1;
        if opp.total_cost <= dec!(0) || opp.total_cost >= dec!(1.2) {
            self.register_failed_trade("invalid_total_cost");
            return;
        }

        if opp.eth_leg_ask_size < dec!(1) || opp.btc_leg_ask_size < dec!(1) {
            self.stats.low_liquidity_skips += 1;
            self.register_failed_trade("entry_liquidity_too_low");
            return;
        }

        let shares = self.stake_per_trade / opp.total_cost;
        if shares > opp.eth_leg_ask_size || shares > opp.btc_leg_ask_size {
            self.stats.low_liquidity_skips += 1;
            self.register_failed_trade("insufficient_ask_depth_for_100usd");
            self.stats.failed_legs += 1;
            return;
        }

        let trade = SimulatedTrade {
            id: self.next_id,
            opened_at: std::time::Instant::now(),
            pair_label: opp.pair_label.clone(),
            shares,
            entry_total: opp.total_cost,
            eth_token_id: opp.eth_up_token_id.clone(),
            btc_token_id: opp.btc_down_token_id.clone(),
        };
        self.next_id += 1;
        self.stats.opened_trades += 1;
        *self
            .stats
            .by_pair_opened
            .entry(trade.pair_label.clone())
            .or_insert(0) += 1;
        self.open_positions.push(trade);
    }

    fn settle_due_positions(&mut self, snapshot: &monitor::MarketSnapshot) {
        let mut remaining = Vec::with_capacity(self.open_positions.len());
        let open_positions = std::mem::take(&mut self.open_positions);

        for trade in open_positions {
            if trade.opened_at.elapsed() < Duration::from_secs(self.hold_seconds) {
                remaining.push(trade);
                continue;
            }

            let Some(eth_bid) = token_bid(snapshot, &trade.eth_token_id) else {
                self.stats.missing_data_events += 1;
                self.stats.failed_legs += 1;
                self.register_failed_trade("missing_eth_bid_on_exit");
                continue;
            };

            let Some(btc_bid) = token_bid(snapshot, &trade.btc_token_id) else {
                self.stats.missing_data_events += 1;
                self.stats.failed_legs += 1;
                self.register_failed_trade("missing_btc_bid_on_exit");
                continue;
            };

            let exit_total = eth_bid + btc_bid;
            let pnl = (exit_total - trade.entry_total) * trade.shares;
            self.stats.closed_trades += 1;
            self.stats.gross_pnl_usd += pnl;

            if pnl > dec!(0) {
                self.stats.wins += 1;
            } else {
                self.stats.losses += 1;
            }

            info!(
                "🧪 [SIM] Closed trade #{} | {} | entry={:.4} exit={:.4} shares={:.2} pnl=${:.2}",
                trade.id, trade.pair_label, trade.entry_total, exit_total, trade.shares, pnl
            );
        }

        self.open_positions = remaining;
    }

    fn register_failed_trade(&mut self, reason: &str) {
        self.stats.failed_trades += 1;
        *self
            .stats
            .by_failure_reason
            .entry(reason.to_string())
            .or_insert(0) += 1;
    }

    fn final_report(&self) -> String {
        let avg_pnl = if self.stats.closed_trades > 0 {
            self.stats.gross_pnl_usd / Decimal::from(self.stats.closed_trades)
        } else {
            dec!(0)
        };
        let win_rate = if self.stats.closed_trades > 0 {
            (self.stats.wins as f64 / self.stats.closed_trades as f64) * 100.0
        } else {
            0.0
        };

        format!(
            "\n================ SIMULATION REPORT ================\n\
Mode: LIVE WS feed + simulated fills\n\
Stake per trade: ${:.2}\n\
Hold period: {}s\n\
Signals seen: {}\n\
Trades opened: {}\n\
Trades closed: {}\n\
Wins: {}\n\
Losses: {}\n\
Failed trades: {}\n\
Failed legs: {}\n\
Missing data events: {}\n\
Low-liquidity skips: {}\n\
Open positions left (force-closed as unresolved): {}\n\
Gross PnL: ${:.2}\n\
Average PnL/trade: ${:.2}\n\
Win rate: {:.2}%\n\
Failure reasons: {:?}\n\
Opened by pair: {:?}\n\
Fail-leg handling: If only one leg can be priced at exit, trade is marked failed_leg and excluded from PnL to avoid fake marking.\n\
Mishap handling: Missing bids / bad totals / low depth are categorized, counted, and surfaced above.\n\
===================================================",
            self.stake_per_trade,
            self.hold_seconds,
            self.stats.seen_signals,
            self.stats.opened_trades,
            self.stats.closed_trades,
            self.stats.wins,
            self.stats.losses,
            self.stats.failed_trades,
            self.stats.failed_legs,
            self.stats.missing_data_events,
            self.stats.low_liquidity_skips,
            self.open_positions.len(),
            self.stats.gross_pnl_usd,
            avg_pnl,
            win_rate,
            self.stats.by_failure_reason,
            self.stats.by_pair_opened,
        )
    }
}

fn token_bid(snapshot: &monitor::MarketSnapshot, token_id: &str) -> Option<Decimal> {
    let candidates = [
        snapshot.eth_market.up_token.as_ref(),
        snapshot.eth_market.down_token.as_ref(),
        snapshot.btc_market.up_token.as_ref(),
        snapshot.btc_market.down_token.as_ref(),
    ];

    for candidate in candidates.into_iter().flatten() {
        if candidate.token_id == token_id {
            return candidate.bid;
        }
    }
    None
}

// ─── Token ID extraction ──────────────────────────────────────────────────────

/// Fetch REAL 77-digit token IDs from the CLOB API.
/// The Gamma API clobTokenIds field is TRUNCATED and causes WS to receive nothing.
/// Only the CLOB API /markets/{conditionId} returns the full correct IDs.
async fn extract_token_ids(condition_id: &str) -> Result<(String, String)> {
    let clob_url = format!("https://clob.polymarket.com/markets/{}", condition_id);
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;

    let body = http.get(&clob_url).send().await?.text().await?;
    let json: serde_json::Value = serde_json::from_str(&body)?;

    let tokens = json["tokens"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("CLOB /markets/{} missing 'tokens' array", condition_id))?;

    if tokens.len() < 2 {
        anyhow::bail!(
            "CLOB market {} has only {} tokens",
            condition_id,
            tokens.len()
        );
    }

    let up_id = tokens[0]["token_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing token_id[0]"))?
        .to_string();

    let down_id = tokens[1]["token_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing token_id[1]"))?
        .to_string();

    if up_id.len() < 30 || down_id.len() < 30 {
        anyhow::bail!(
            "Token IDs too short ({}/{} chars) — CLOB API may have returned wrong data",
            up_id.len(),
            down_id.len()
        );
    }

    Ok((up_id, down_id))
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    logging::init_logging()?;

    info!("🚀 Starting Polymarket WS Arbitrage Bot");

    let args = Args::parse();
    let config = Config::load(&args.config)?;

    // ── Provider ──────────────────────────────────────────────────────────────
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL missing");
    let provider = Arc::new(Provider::<Http>::try_from(&rpc_url)?);

    // ── Wallet ────────────────────────────────────────────────────────────────
    let private_key = std::env::var("PRIVATE_KEY").expect("PRIVATE_KEY missing");
    let proxy_wallet = std::env::var("PROXY_WALLET").expect("PROXY_WALLET missing");
    let signer = WalletSigner::new(&private_key, 137)?;

    info!("🔑 Signer loaded | Proxy wallet: {}", proxy_wallet);

    // ── Execution mode ────────────────────────────────────────────────────────
    let execution_mode = std::env::var("EXECUTION_MODE").unwrap_or_else(|_| "executor".into());

    if execution_mode == "direct" {
        verify_allowances(provider.clone(), &proxy_wallet).await?;
        info!("✅ Allowances verified (direct mode)");
    } else {
        info!("ℹ️  EXECUTION_MODE=executor — skipping direct allowance preflight");
    }

    // ── API credentials ───────────────────────────────────────────────────────
    let api_key = std::env::var("POLY_API_KEY").expect("POLY_API_KEY missing");
    let api_secret = std::env::var("POLY_API_SECRET").expect("POLY_API_SECRET missing");
    let api_passphrase = std::env::var("POLY_API_PASSPHRASE").expect("POLY_API_PASSPHRASE missing");

    let read_only = std::env::var("READ_ONLY")
        .unwrap_or_else(|_| "true".to_string())
        .parse::<bool>()
        .unwrap_or(true);

    // ── CLOB client (direct mode only) ───────────────────────────────────────
    let clob = if execution_mode == "direct" {
        Some(Arc::new(
            ClobClient::new(
                &rpc_url,
                &private_key,
                &proxy_wallet,
                api_key.clone(),
                api_secret.clone(),
                api_passphrase.clone(),
            )
            .await?,
        ))
    } else {
        None
    };

    // ── Python executor (executor mode) ───────────────────────────────────────
    let executor = if execution_mode == "executor" {
        let url =
            std::env::var("EXECUTOR_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".to_string());
        let client = ExecutorClient::new(url)?;
        client.healthcheck().await?;
        info!("✅ Executor service reachable");
        Some(client)
    } else {
        None
    };

    // ── REST API client ───────────────────────────────────────────────────────
    let api = Arc::new(PolymarketClient::new(
        config.polymarket.gamma_api_url.clone(),
        config.polymarket.clob_api_url.clone(),
        api_key,
        api_secret,
        api_passphrase,
        read_only,
        clob.clone(),
    ));

    let sports_mode = env_bool_any_optional(&["SPORTS_MODE"]).unwrap_or(false);
    if sports_mode {
        info!("🏈 SPORTS_MODE=true — running sports-only arbitrage scanner");
        run_sports_mode(api.clone(), executor.clone()).await?;
        return Ok(());
    }

    // ── Core objects ──────────────────────────────────────────────────────────
    let detector = Arc::new(ArbitrageDetector::new(config.trading.min_profit_threshold));

    let wallet_config = WalletConfig {
        private_key: Some(private_key.clone()),
        chain_id: 137,
        proxy_wallet: proxy_wallet.clone(),
    };

    let trader = Arc::new(Trader::new(
        api.clone(),
        clob,
        executor,
        config.trading.clone(),
        wallet_config,
        Some(signer),
    ));

    let ws_url = config.polymarket.ws_url.clone();
    let mut current_period = current_15m_period();
    let simulation_mode = env_bool_any_optional(&["SIMULATION_MODE", "SIM_MODE"]).unwrap_or(false);
    let hold_seconds = std::env::var("SIM_HOLD_SECONDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20)
        .max(1);

    let sim_engine = Arc::new(Mutex::new(SimulationEngine::new(hold_seconds, dec!(100))));
    let (stop_tx, stop_rx) = watch::channel(false);

    if simulation_mode {
        info!(
            "🧪 SIMULATION_MODE enabled (stake=${:.2}, hold={}s)",
            100.0, hold_seconds
        );
        info!("⌨️  Press SPACE (then Enter in some terminals) to stop simulation and print a detailed report");

        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin();
            let mut buf = [0_u8; 1];
            loop {
                if stdin.read_exact(&mut buf).is_ok() && buf[0] == b' ' {
                    let _ = stop_tx.send(true);
                    break;
                }
            }
        });
    }

    // ── Main outer loop — restarts every 15 minutes ───────────────────────────
    loop {
        let pair_cfg = selected_pair_config()?;
        info!(
            "🔍 Discovering 15m markets for {}-{}",
            pair_cfg.left_name, pair_cfg.right_name
        );

        let (left_market, right_market) = discover_markets(&api, pair_cfg).await?;

        // Extract REAL token IDs via CLOB API (Gamma API returns truncated IDs)
        let (btc_up_id, btc_down_id) = extract_token_ids(&left_market.condition_id).await?;
        let (eth_up_id, eth_down_id) = extract_token_ids(&right_market.condition_id).await?;

        info!(
            "✅ {} market: {} | UP={} DOWN={}",
            pair_cfg.left_name,
            left_market.slug,
            &btc_up_id[..16],
            &btc_down_id[..16]
        );
        info!(
            "✅ {} market: {} | UP={} DOWN={}",
            pair_cfg.right_name,
            right_market.slug,
            &eth_up_id[..16],
            &eth_down_id[..16]
        );

        let tokens = WatchedTokens {
            btc_up_id: btc_up_id.clone(),
            btc_down_id: btc_down_id.clone(),
            eth_up_id: eth_up_id.clone(),
            eth_down_id: eth_down_id.clone(),
            eth_condition_id: right_market.condition_id.clone(),
            btc_condition_id: left_market.condition_id.clone(),
            eth_name: pair_cfg.right_name.to_string(),
            btc_name: pair_cfg.left_name.to_string(),
        };

        // ── Spawn WS feed for this period ──────────────────────────────────
        let cache = PriceCache::new();
        let ws_rx = spawn_ws_feed(ws_url.clone(), tokens.all_ids(), cache.clone());

        info!("🌐 WS feed spawned — waiting for first book snapshots…");

        let mut monitor = WsMarketMonitor::new(cache, tokens, ws_rx);

        // ── WS-driven monitoring task ──────────────────────────────────────
        let sim_engine_for_monitor = sim_engine.clone();
        let monitor_handle = tokio::spawn({
            let detector = detector.clone();
            let trader = trader.clone();
            let sim_engine = sim_engine_for_monitor.clone();

            async move {
                monitor
                    .start(move |snapshot| {
                        let detector = detector.clone();
                        let trader = trader.clone();
                        let sim_engine = sim_engine.clone();
                        let simulation_mode = simulation_mode;

                        async move {
                            let opportunities = detector.detect_opportunities(&snapshot);

                            if simulation_mode {
                                let mut engine = sim_engine.lock().await;
                                for opp in &opportunities {
                                    engine.register_opportunity(opp);
                                }
                                engine.settle_due_positions(&snapshot);
                                return;
                            }

                            if !opportunities.is_empty() {
                                info!("🔔 Found {} opportunity(ies)!", opportunities.len());
                            }

                            for (i, opp) in opportunities.iter().enumerate() {
                                info!(
                                    "📋 Processing opportunity {} of {}",
                                    i + 1,
                                    opportunities.len()
                                );
                                match trader.execute_arbitrage(opp).await {
                                    Ok(_) => info!("✅ Opportunity {} handled", i + 1),
                                    Err(e) => warn!("❌ Opportunity {} failed: {}", i + 1, e),
                                }
                            }
                        }
                    })
                    .await;
            }
        });

        // ── Wait for 15m period rollover ───────────────────────────────────
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            if simulation_mode && *stop_rx.borrow() {
                info!("🛑 Space pressed — stopping simulation now");
                monitor_handle.abort();

                let report = {
                    let engine = sim_engine.lock().await;
                    engine.final_report()
                };
                println!("{}", report);
                return Ok(());
            }

            let new_period = current_15m_period();
            if new_period != current_period {
                info!("⏰ 15m rollover — restarting WS feed for new markets");
                current_period = new_period;
                monitor_handle.abort(); // also kills the WS task via Drop
                break;
            }
        }
    }
}

// ─── Market discovery ─────────────────────────────────────────────────────────

async fn discover_markets(
    api: &PolymarketClient,
    pair_cfg: PairConfig,
) -> Result<(domain::Market, domain::Market)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    let mut seen = std::collections::HashSet::new();

    let left = discover_market(
        api,
        pair_cfg.left_name,
        pair_cfg.left_prefix,
        now,
        &mut seen,
    )
    .await?;
    seen.insert(left.condition_id.clone());
    let right = discover_market(
        api,
        pair_cfg.right_name,
        pair_cfg.right_prefix,
        now,
        &mut seen,
    )
    .await?;

    Ok((left, right))
}

async fn discover_market(
    api: &PolymarketClient,
    name: &str,
    prefix: &str,
    now: u64,
    seen: &mut std::collections::HashSet<String>,
) -> Result<domain::Market> {
    let base = (now / 900) * 900;

    for i in 0..=3 {
        let ts = base - i * 900;
        let slug = format!("{}-updown-15m-{}", prefix, ts);

        if let Ok(market) = api.get_market_by_slug(&slug).await {
            if !seen.contains(&market.condition_id) && market.active {
                info!("Found {} market: {}", name, market.slug);
                return Ok(market);
            }
        }
    }

    anyhow::bail!("No active {} market found", name)
}

async fn run_sports_mode(
    api: Arc<PolymarketClient>,
    executor: Option<ExecutorClient>,
) -> Result<()> {
    let gamma_url = api.gamma_url.clone();
    let max_sum = std::env::var("ARBITRAGE_MAX_SUM")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.985);
    let min_size = std::env::var("MIN_SHARES")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(5.0);
    let scan_interval = Duration::from_millis(
        std::env::var("SPORTS_SCAN_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(250),
    );
    let max_pages = std::env::var("SPORTS_MAX_DISCOVERY_PAGES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(20);
    let page_concurrency = std::env::var("SPORTS_DISCOVERY_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(8);
    let scan_concurrency = std::env::var("SPORTS_SCAN_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(128);
    let cooldown = Duration::from_millis(
        std::env::var("OPPORTUNITY_COOLDOWN_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5000),
    );
    let sports_auto_trade = env_bool_any_optional(&["SPORTS_AUTO_TRADE"]).unwrap_or(false);
    let size_usdc = std::env::var("SPORTS_TRADE_SIZE_USDC")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(5.0);

    info!("🏈 Discovering active sports markets in parallel...");
    let markets = discover_sports_markets(&gamma_url, max_pages, page_concurrency)
        .await
        .context("failed to discover sports markets")?;
    info!(
        "✅ Loaded {} active 2-outcome sports markets | scan interval={}ms",
        markets.len(),
        scan_interval.as_millis()
    );

    let mut last_seen: HashMap<String, Instant> = HashMap::new();

    loop {
        let opportunities =
            scan_sports_once(api.clone(), &markets, max_sum, min_size, scan_concurrency).await;

        for opp in opportunities {
            let key = opp.market.slug.clone();
            if let Some(ts) = last_seen.get(&key) {
                if ts.elapsed() < cooldown {
                    continue;
                }
            }
            last_seen.insert(key, Instant::now());

            info!(
                "⚡ SPORTS ARB {} [{}] {} | {}={:.4} {}={:.4} SUM={:.4} EDGE={:.4}",
                opp.market.slug,
                opp.market
                    .sports_market_type
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                opp.market.question,
                opp.market.outcome_a_label,
                opp.outcome_a_ask,
                opp.market.outcome_b_label,
                opp.outcome_b_ask,
                opp.sum,
                opp.edge
            );

            if sports_auto_trade {
                let Some(exec) = executor.as_ref() else {
                    warn!("SPORTS_AUTO_TRADE=true but executor is unavailable (set EXECUTION_MODE=executor)");
                    continue;
                };

                let (a_res, b_res) = tokio::join!(
                    exec.execute_order(
                        &opp.market.outcome_a_token,
                        domain::order::Side::Buy,
                        opp.outcome_a_ask,
                        size_usdc
                    ),
                    exec.execute_order(
                        &opp.market.outcome_b_token,
                        domain::order::Side::Buy,
                        opp.outcome_b_ask,
                        size_usdc
                    )
                );

                match (a_res, b_res) {
                    (Ok(a), Ok(b)) => info!(
                        "✅ placed sports legs market={} {}={:?} {}={:?}",
                        opp.market.slug,
                        opp.market.outcome_a_label,
                        a.order_id,
                        opp.market.outcome_b_label,
                        b.order_id
                    ),
                    (a, b) => warn!(
                        "❌ sports execution failed for {} | {}={:?} {}={:?}",
                        opp.market.slug,
                        opp.market.outcome_a_label,
                        a,
                        opp.market.outcome_b_label,
                        b
                    ),
                }
            }
        }

        tokio::time::sleep(scan_interval).await;
    }
}

async fn scan_sports_once(
    api: Arc<PolymarketClient>,
    markets: &[SportsMarket],
    max_sum: f64,
    min_size: f64,
    concurrency: usize,
) -> Vec<SportsOpportunity> {
    stream::iter(markets.iter().cloned())
        .map(|market| {
            let api = api.clone();
            async move {
                let (a_book, b_book) = tokio::join!(
                    crate::execution::orderbook::fetch_orderbook(&api, &market.outcome_a_token),
                    crate::execution::orderbook::fetch_orderbook(&api, &market.outcome_b_token)
                );

                let a_book = a_book.ok()?;
                let b_book = b_book.ok()?;

                let (a_ask, a_size) = a_book.cheapest_ask_with_min_size(min_size)?;
                let (b_ask, b_size) = b_book.cheapest_ask_with_min_size(min_size)?;
                if a_size < min_size || b_size < min_size {
                    return None;
                }

                let sum = a_ask + b_ask;
                if sum >= max_sum {
                    return None;
                }

                Some(SportsOpportunity {
                    market,
                    outcome_a_ask: a_ask,
                    outcome_b_ask: b_ask,
                    sum,
                    edge: 1.0 - sum,
                })
            }
        })
        .buffer_unordered(concurrency)
        .filter_map(async move |x| x)
        .collect()
        .await
}

async fn discover_sports_markets(
    gamma_url: &str,
    max_pages: usize,
    concurrency: usize,
) -> Result<Vec<SportsMarket>> {
    let http = Client::new();
    let market_types = fetch_sports_market_types(&http, gamma_url)
        .await
        .unwrap_or_default();

    let offsets: Vec<usize> = (0..max_pages).map(|i| i * 100).collect();

    let pages: Vec<Vec<SportsMarket>> = stream::iter(offsets)
        .map(|offset| {
            let http = http.clone();
            let base = gamma_url.to_string();
            let market_types = market_types.clone();
            async move {
                fetch_sports_page_markets(&http, &base, offset, &market_types)
                    .await
                    .unwrap_or_default()
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for market in pages.into_iter().flatten() {
        if seen.insert(market.condition_id.clone()) {
            out.push(market);
        }
    }
    Ok(out)
}

async fn fetch_sports_market_types(http: &Client, gamma_url: &str) -> Result<HashSet<String>> {
    let url = format!("{}/sports/market-types", gamma_url.trim_end_matches('/'));
    let resp = http.get(url).send().await?.error_for_status()?;
    let body: SportsMarketTypesResponse = resp.json().await?;
    Ok(body
        .market_types
        .into_iter()
        .map(|s| s.to_lowercase())
        .collect())
}

async fn fetch_sports_page_markets(
    http: &Client,
    gamma_url: &str,
    offset: usize,
    market_types: &HashSet<String>,
) -> Result<Vec<SportsMarket>> {
    let url = format!(
        "{}/events?active=true&closed=false&limit=100&offset={}",
        gamma_url.trim_end_matches('/'),
        offset
    );

    let events: Vec<Value> = http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut markets = Vec::new();

    for event in events {
        let event_markets = event
            .get("markets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for m in event_markets {
            if let Some(parsed) = parse_sports_market(&m, market_types) {
                markets.push(parsed);
            }
        }
    }

    Ok(markets)
}

fn parse_sports_market(v: &Value, market_types: &HashSet<String>) -> Option<SportsMarket> {
    if !v.get("active")?.as_bool()? || v.get("closed")?.as_bool()? {
        return None;
    }

    let sports_type = v
        .get("sportsMarketType")
        .or_else(|| v.get("sports_market_type"))
        .or_else(|| v.get("marketType"))
        .and_then(Value::as_str)
        .map(|s| s.to_lowercase());

    if !market_types.is_empty() {
        if let Some(t) = sports_type.as_ref() {
            if !market_types.contains(t) {
                return None;
            }
        }
    }

    let condition_id = v.get("conditionId")?.as_str()?.to_string();
    let slug = v.get("slug")?.as_str()?.to_string();
    let question = v
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let (outcome_a_label, outcome_a_token, outcome_b_label, outcome_b_token) =
        extract_sports_binary_outcomes(v)?;

    Some(SportsMarket {
        slug,
        condition_id,
        question,
        outcome_a_label,
        outcome_a_token,
        outcome_b_label,
        outcome_b_token,
        sports_market_type: sports_type,
    })
}

fn extract_sports_binary_outcomes(v: &Value) -> Option<(String, String, String, String)> {
    if let Some(tokens) = v.get("tokens").and_then(Value::as_array) {
        let parsed = tokens
            .iter()
            .filter_map(|t| {
                let label = t.get("outcome")?.as_str()?.to_string();
                let token = t
                    .get("tokenId")
                    .or_else(|| t.get("token_id"))
                    .and_then(Value::as_str)?
                    .to_string();
                Some((label, token))
            })
            .take(2)
            .collect::<Vec<_>>();

        if parsed.len() == 2 {
            return Some((
                parsed[0].0.clone(),
                parsed[0].1.clone(),
                parsed[1].0.clone(),
                parsed[1].1.clone(),
            ));
        }
    }

    let outcomes = v
        .get("outcomes")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())?;

    let token_ids = v
        .get("clobTokenIds")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())?;

    if outcomes.len() >= 2 && token_ids.len() >= 2 {
        return Some((
            outcomes[0].clone(),
            token_ids[0].clone(),
            outcomes[1].clone(),
            token_ids[1].clone(),
        ));
    }

    None
}
