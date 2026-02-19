/// main.rs — WS-driven Polymarket arbitrage bot
///
/// Flow:
///   1. Discover current markets via REST (pair mode: 15m, BTC_5_MIN mode: 5m)
///   2. Spawn WS feed subscribing to all 4 tokens
///   3. WsMarketMonitor reacts to book updates (< 1 ms)
///   4. ArbitrageDetector checks opportunity on every update
///   5. Trader executes BOTH legs in parallel (tokio::join!)
///   6. On market-window rollover: restart WS feed + monitor for new markets
use polymarket_15m_arbitrage_bot::*;

use anyhow::Result;
use clap::Parser;
use config::{Args, Config};
use log::{info, warn};
use rust_decimal::prelude::ToPrimitive;

use crate::domain::ArbitrageOpportunity;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::config::WalletConfig;
use cache::PriceCache;
use client::PolymarketClient;
use ethers::providers::{Http, Provider};
use execution::{clob_client::ClobClient, ExecutorClient, Trader};
use monitor::{WatchedTokens, WsMarketMonitor};
use strategy::ArbitrageDetector;
use wallet::allowance::verify_allowances;
use wallet::signer::WalletSigner;
use ws::{spawn_ws_feed, sports::spawn_live_slug_tracker};

// ─── Pair config ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct PairConfig {
    left_name: &'static str,
    left_prefix: &'static str,
    right_name: &'static str,
    right_prefix: &'static str,
}

#[derive(Clone)]
enum BotMode {
    CrossPair(PairConfig),
    BtcFiveMinute,
    SportsLive,
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

fn opportunity_signature(opp: &ArbitrageOpportunity) -> String {
    format!(
        "{}|{}|{}|{:.4}|{:.4}",
        opp.pair_label,
        opp.eth_condition_id,
        opp.btc_condition_id,
        opp.eth_up_price.to_f64().unwrap_or_default(),
        opp.btc_down_price.to_f64().unwrap_or_default()
    )
}

fn selected_mode() -> Result<BotMode> {
    if env_bool_any_optional(&["SPORTS_MOD", "SPORTS_MODE"]).unwrap_or(false) {
        return Ok(BotMode::SportsLive);
    }

    if env_bool_any_optional(&["BTC_5_MIN"]).unwrap_or(false) {
        return Ok(BotMode::BtcFiveMinute);
    }

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
        return Ok(BotMode::CrossPair(PairConfig {
            left_name: "BTC",
            left_prefix: "btc",
            right_name: "ETH",
            right_prefix: "eth",
        }));
    }
    if use_btc_sol {
        return Ok(BotMode::CrossPair(PairConfig {
            left_name: "BTC",
            left_prefix: "btc",
            right_name: "SOL",
            right_prefix: "sol",
        }));
    }
    Ok(BotMode::CrossPair(PairConfig {
        left_name: "BTC",
        left_prefix: "btc",
        right_name: "XRP",
        right_prefix: "xrp",
    }))
}

// ─── market-window period helper ──────────────────────────────────────────────

fn current_window_period(window_secs: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    (now / window_secs) * window_secs
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

async fn prewarm_next_period_markets(
    api: Arc<PolymarketClient>,
    pair_cfg: PairConfig,
    next_period: u64,
    market_window_slug: &str,
) {
    for (name, prefix) in [
        (pair_cfg.left_name, pair_cfg.left_prefix),
        (pair_cfg.right_name, pair_cfg.right_prefix),
    ] {
        let slug = format!("{}-updown-{}-{}", prefix, market_window_slug, next_period);
        match api.get_market_by_slug(&slug).await {
            Ok(market) => info!(
                "🔥 Prewarmed next {} market slug={} active={}",
                name, market.slug, market.active
            ),
            Err(err) => warn!("⚠️ Prewarm miss for {} slug={}: {}", name, slug, err),
        }
    }
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
    let recent_opportunities: Arc<Mutex<HashMap<String, Instant>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let ws_url = config.polymarket.ws_url.clone();
    let mode = selected_mode()?;
    let (window_secs, window_slug, window_label) = match &mode {
        BotMode::CrossPair(_) => (900, "15m", "15m"),
        BotMode::BtcFiveMinute => (300, "5m", "5m"),
        BotMode::SportsLive => (900, "15m", "15m"),
    };

    let mut active_period = if window_secs > 0 {
        current_window_period(window_secs)
    } else {
        0
    };

    let dedupe_ms: u64 = std::env::var("OPPORTUNITY_DEDUPE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);

    if matches!(mode, BotMode::SportsLive) {
        return run_sports_live_mode(
            api,
            trader,
            detector,
            ws_url,
            recent_opportunities,
            dedupe_ms,
        )
        .await;
    }

    // ── Main outer loop — restarts every market window ────────────────────────
    loop {
        let mode = selected_mode()?;
        let (left_market, right_market, right_name) = match mode.clone() {
            BotMode::CrossPair(pair_cfg) => {
                info!(
                    "🔍 Discovering {} markets for {}-{}",
                    window_label, pair_cfg.left_name, pair_cfg.right_name
                );
                let (left, right) =
                    discover_markets(&api, pair_cfg, window_secs, window_slug).await?;
                (left, right, pair_cfg.right_name.to_string())
            }
            BotMode::BtcFiveMinute => {
                info!(
                    "🔍 BTC_5_MIN=true | discovering active BTC {} market for in-market UP/DOWN arb",
                    window_label
                );
                let btc =
                    discover_single_market(&api, "BTC", "btc", window_secs, window_slug).await?;
                (btc.clone(), btc, "BTC".to_string())
            }
            BotMode::SportsLive => unreachable!("sports mode handled in dedicated path"),
        };

        // Extract REAL token IDs via CLOB API (Gamma API returns truncated IDs)
        let (btc_up_id, btc_down_id) = extract_token_ids(&left_market.condition_id).await?;
        let (eth_up_id, eth_down_id) = extract_token_ids(&right_market.condition_id).await?;

        info!(
            "✅ {} market: {} | UP={} DOWN={}",
            "BTC",
            left_market.slug,
            &btc_up_id[..16],
            &btc_down_id[..16]
        );
        info!(
            "✅ {} market: {} | UP={} DOWN={}",
            right_name,
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
            eth_name: right_name,
            btc_name: "BTC".to_string(),
            id_set: [
                btc_up_id.clone(),
                btc_down_id.clone(),
                eth_up_id.clone(),
                eth_down_id.clone(),
            ]
            .into_iter()
            .collect(),
        };

        let btc_up_ref = Arc::new(btc_up_id.clone());

        // ── Spawn WS feed for this period ──────────────────────────────────
        let cache = PriceCache::new();
        let ws_rx = spawn_ws_feed(ws_url.clone(), tokens.all_ids(), cache.clone());

        info!("🌐 WS feed spawned — waiting for first book snapshots…");

        let mut monitor = WsMarketMonitor::new(cache, tokens, ws_rx);

        // ── WS-driven monitoring task ──────────────────────────────────────
        let mode_for_detector = mode.clone();
        let monitor_handle = tokio::spawn({
            let detector = detector.clone();
            let trader = trader.clone();
            let recent_opportunities = recent_opportunities.clone();

            async move {
                monitor
                    .start(move |snapshot| {
                        let detector = detector.clone();
                        let trader = trader.clone();
                        let recent_opportunities = recent_opportunities.clone();
                        let btc_up_ref = btc_up_ref.clone();
                        let mode_for_snapshot = mode_for_detector.clone();

                        async move {
                            let opportunities = detector.detect_opportunities(&snapshot);

                            if !opportunities.is_empty() {
                                info!("🔔 Found {} opportunity(ies)!", opportunities.len());
                            }

                            for (i, opp) in opportunities.iter().enumerate() {
                                if matches!(mode_for_snapshot, BotMode::BtcFiveMinute)
                                    && opp.eth_up_token_id != *btc_up_ref
                                {
                                    continue;
                                }

                                let sig = opportunity_signature(opp);
                                let now = Instant::now();

                                if dedupe_ms > 0 {
                                    let mut seen = recent_opportunities.lock().await;
                                    if let Some(last_seen) = seen.get(&sig) {
                                        if now.duration_since(*last_seen)
                                            < Duration::from_millis(dedupe_ms)
                                        {
                                            info!(
                                                "🔁 Duplicate opportunity suppressed ({}ms window): {}",
                                                dedupe_ms, opp.pair_label
                                            );
                                            continue;
                                        }
                                    }

                                    seen.insert(sig, now);
                                    seen.retain(|_, ts| {
                                        now.duration_since(*ts)
                                            < Duration::from_millis(dedupe_ms.saturating_mul(4))
                                    });
                                }

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

        // ── Wait for market-window rollover ─────────────────────────────────
        let mut prewarmed_next_period = false;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let secs_until_rollover = window_secs - (now_secs % window_secs);

            if !prewarmed_next_period && secs_until_rollover <= 30 {
                if let BotMode::CrossPair(pair_cfg) = mode {
                    let next_period = active_period + window_secs;
                    let api_clone = api.clone();
                    tokio::spawn(prewarm_next_period_markets(
                        api_clone,
                        pair_cfg,
                        next_period,
                        window_slug,
                    ));
                    info!(
                        "⏰ {}s to rollover — prewarming next-period market discovery",
                        secs_until_rollover
                    );
                }
                prewarmed_next_period = true;
            }

            let new_period = current_window_period(window_secs);
            if new_period != active_period {
                info!(
                    "⏰ {} rollover — restarting WS feed for new markets",
                    window_label
                );
                active_period = new_period;
                monitor_handle.abort(); // also kills the WS task via Drop
                break;
            }
        }
    }
}

#[derive(Clone)]
struct SportsTrackedMarket {
    market: domain::Market,
    up_id: String,
    down_id: String,
}

async fn run_sports_live_mode(
    api: Arc<PolymarketClient>,
    trader: Arc<Trader>,
    detector: Arc<ArbitrageDetector>,
    ws_url: String,
    recent_opportunities: Arc<Mutex<HashMap<String, Instant>>>,
    dedupe_ms: u64,
) -> Result<()> {
    info!("🏟️ SPORTS_MOD=true | discovering all currently live sports markets");
    let live_slug_tracker = spawn_live_slug_tracker();

    let refresh_secs = std::env::var("SPORTS_REFRESH_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60);
    let max_markets = std::env::var("SPORTS_MAX_MARKETS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100);

    loop {
        let live_slugs = { live_slug_tracker.read().await.clone() };

        if live_slugs.is_empty() {
            warn!(
                "🏟️ No live sports events currently in-progress from sports WS; retrying in {}s",
                refresh_secs
            );
            tokio::time::sleep(Duration::from_secs(refresh_secs)).await;
            continue;
        }

        let tracked = discover_live_sports_markets(&api, &live_slugs, max_markets).await?;
        if tracked.is_empty() {
            warn!(
                "🏟️ Live sports slugs found ({}), but no active tradable markets resolved; retrying in {}s",
                live_slugs.len(),
                refresh_secs
            );
            tokio::time::sleep(Duration::from_secs(refresh_secs)).await;
            continue;
        }

        let mut id_set = std::collections::HashSet::new();
        for item in &tracked {
            id_set.insert(item.up_id.clone());
            id_set.insert(item.down_id.clone());
        }

        info!(
            "🏟️ Tracking {} live sports markets ({} tokens)",
            tracked.len(),
            id_set.len()
        );

        let cache = PriceCache::new();
        let ws_rx = spawn_ws_feed(ws_url.clone(), id_set.into_iter().collect(), cache.clone());

        let mut handles = Vec::new();
        for item in tracked {
            let tokens = WatchedTokens {
                btc_up_id: item.up_id.clone(),
                btc_down_id: item.down_id.clone(),
                eth_up_id: item.up_id.clone(),
                eth_down_id: item.down_id.clone(),
                eth_condition_id: item.market.condition_id.clone(),
                btc_condition_id: item.market.condition_id.clone(),
                eth_name: item.market.slug.clone(),
                btc_name: item.market.slug.clone(),
                id_set: [item.up_id.clone(), item.down_id.clone()]
                    .into_iter()
                    .collect(),
            };

            let mut monitor = WsMarketMonitor::new(cache.clone(), tokens, ws_rx.resubscribe());
            let detector = detector.clone();
            let trader = trader.clone();
            let recent_opportunities = recent_opportunities.clone();
            let up_ref = Arc::new(item.up_id.clone());

            let h = tokio::spawn(async move {
                monitor
                    .start(move |snapshot| {
                        let detector = detector.clone();
                        let trader = trader.clone();
                        let recent_opportunities = recent_opportunities.clone();
                        let up_ref = up_ref.clone();

                        async move {
                            let opportunities = detector.detect_opportunities(&snapshot);
                            for opp in opportunities.iter() {
                                if opp.eth_up_token_id != *up_ref {
                                    continue;
                                }

                                let sig = opportunity_signature(opp);
                                let now = Instant::now();
                                if dedupe_ms > 0 {
                                    let mut seen = recent_opportunities.lock().await;
                                    if let Some(last_seen) = seen.get(&sig) {
                                        if now.duration_since(*last_seen)
                                            < Duration::from_millis(dedupe_ms)
                                        {
                                            continue;
                                        }
                                    }
                                    seen.insert(sig, now);
                                }

                                if let Err(err) = trader.execute_arbitrage(opp).await {
                                    warn!("❌ Sports opportunity failed: {}", err);
                                }
                            }
                        }
                    })
                    .await;
            });
            handles.push(h);
        }

        tokio::time::sleep(Duration::from_secs(refresh_secs)).await;
        for h in handles {
            h.abort();
        }
    }
}

async fn discover_live_sports_markets(
    api: &PolymarketClient,
    live_slugs: &std::collections::HashSet<String>,
    max_markets: usize,
) -> Result<Vec<SportsTrackedMarket>> {
    let mut out = Vec::new();

    for slug in live_slugs {
        if out.len() >= max_markets {
            break;
        }

        let markets = match api.get_event_markets_by_slug(slug).await {
            Ok(markets) => markets,
            Err(err) => {
                warn!(
                    "🏟️ Could not resolve live slug '{}' to markets: {}",
                    slug, err
                );
                continue;
            }
        };

        for market in markets {
            if out.len() >= max_markets {
                break;
            }

            if !market.active || market.closed {
                continue;
            }

            let (up_id, down_id) = match extract_token_ids(&market.condition_id).await {
                Ok(ids) => ids,
                Err(err) => {
                    warn!(
                        "🏟️ Failed token extraction for sports market {}: {}",
                        market.condition_id, err
                    );
                    continue;
                }
            };

            out.push(SportsTrackedMarket {
                market,
                up_id,
                down_id,
            });
        }
    }

    Ok(out)
}

// ─── Market discovery ─────────────────────────────────────────────────────────

async fn discover_markets(
    api: &PolymarketClient,
    pair_cfg: PairConfig,
    window_secs: u64,
    window_slug: &str,
) -> Result<(domain::Market, domain::Market)> {
    let mut seen = std::collections::HashSet::new();

    let left = discover_market(
        api,
        pair_cfg.left_name,
        pair_cfg.left_prefix,
        window_secs,
        window_slug,
        &mut seen,
    )
    .await?;
    seen.insert(left.condition_id.clone());
    let right = discover_market(
        api,
        pair_cfg.right_name,
        pair_cfg.right_prefix,
        window_secs,
        window_slug,
        &mut seen,
    )
    .await?;

    Ok((left, right))
}

async fn discover_single_market(
    api: &PolymarketClient,
    name: &str,
    prefix: &str,
    window_secs: u64,
    window_slug: &str,
) -> Result<domain::Market> {
    let mut seen = std::collections::HashSet::new();
    discover_market(api, name, prefix, window_secs, window_slug, &mut seen).await
}

async fn discover_market(
    api: &PolymarketClient,
    name: &str,
    prefix: &str,
    window_secs: u64,
    window_slug: &str,
    seen: &mut std::collections::HashSet<String>,
) -> Result<domain::Market> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let base = (now / window_secs) * window_secs;

    for i in 0..=3 {
        let ts = base - i * window_secs;
        let slug = format!("{}-updown-{}-{}", prefix, window_slug, ts);

        if let Ok(market) = api.get_market_by_slug(&slug).await {
            if !seen.contains(&market.condition_id) && market.active {
                info!("Found {} market: {}", name, market.slug);
                return Ok(market);
            }
        }
    }

    anyhow::bail!("No active {} market found", name)
}
