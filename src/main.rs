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

use anyhow::Result;
use clap::Parser;
use config::{Args, Config};
use log::{info, warn};
use std::sync::Arc;

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

// ─── Token ID extraction ──────────────────────────────────────────────────────

fn extract_token_ids(market: &domain::Market) -> Result<(String, String)> {
    let raw = market
        .clob_token_ids
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("market {} missing clob_token_ids", market.slug))?;
    let ids: Vec<String> = serde_json::from_str(raw)?;
    if ids.len() < 2 {
        anyhow::bail!(
            "market {} has only {} token IDs (need 2)",
            market.slug,
            ids.len()
        );
    }
    Ok((ids[0].clone(), ids[1].clone()))
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

    let ws_url = config.polymarket.ws_url.clone();
    let mut current_period = current_15m_period();

    // ── Main outer loop — restarts every 15 minutes ───────────────────────────
    loop {
        let pair_cfg = selected_pair_config()?;
        info!(
            "🔍 Discovering 15m markets for {}-{}",
            pair_cfg.left_name, pair_cfg.right_name
        );

        let (left_market, right_market) = discover_markets(&api, pair_cfg).await?;

        // Extract the 4 token IDs we will subscribe to
        let (btc_up_id, btc_down_id) = extract_token_ids(&left_market)?;
        let (eth_up_id, eth_down_id) = extract_token_ids(&right_market)?;

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
        let monitor_handle = tokio::spawn({
            let detector = detector.clone();
            let trader = trader.clone();

            async move {
                monitor
                    .start(move |snapshot| {
                        let detector = detector.clone();
                        let trader = trader.clone();

                        async move {
                            let opportunities = detector.detect_opportunities(&snapshot);

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
