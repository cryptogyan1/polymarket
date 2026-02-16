use polymarket_15m_arbitrage_bot::*;

use anyhow::Result;
use clap::Parser;
use config::{Args, Config};
use log::{info, warn}; // ← CHANGED: Added 'warn' import
use std::sync::Arc;

use crate::config::WalletConfig;
use cache::PriceCache;
use client::PolymarketClient;
use ethers::providers::{Http, Provider};
use execution::{clob_client::ClobClient, ExecutorClient, Trader};
use monitor::MarketMonitor;
use strategy::ArbitrageDetector;
use wallet::allowance::verify_allowances;
use wallet::signer::WalletSigner;

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
    let btc_eth_raw = env_bool_any_optional(&["PAIR_BTC_ETH", "BTC_ETH", "BTC-ETH"]);
    let btc_sol_raw = env_bool_any_optional(&["PAIR_BTC_SOL", "BTC_SOL", "BTC-SOL"]);
    let btc_xrp_raw = env_bool_any_optional(&["PAIR_BTC_XRP", "BTC_XRP", "BTC-XRP"]);

    let any_explicit = btc_eth_raw.is_some() || btc_sol_raw.is_some() || btc_xrp_raw.is_some();

    let btc_eth = btc_eth_raw.unwrap_or(!any_explicit);
    let btc_sol = btc_sol_raw.unwrap_or(false);
    let btc_xrp = btc_xrp_raw.unwrap_or(false);

    let enabled_count = [btc_eth, btc_sol, btc_xrp]
        .iter()
        .filter(|enabled| **enabled)
        .count();

    if enabled_count != 1 {
        anyhow::bail!(
            "exactly one pair toggle must be enabled: PAIR_BTC_ETH, PAIR_BTC_SOL, PAIR_BTC_XRP"
        );
    }

    if btc_eth {
        return Ok(PairConfig {
            left_name: "BTC",
            left_prefix: "btc",
            right_name: "ETH",
            right_prefix: "eth",
        });
    }

    if btc_sol {
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

// ===============================
// TIME HELPERS
// ===============================
fn current_15m_period() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    (now / 900) * 900
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    info!("🚀 Starting Polymarket Arbitrage Bot");

    let args = Args::parse();
    let config = Config::load(&args.config)?;

    // ===============================
    // PROVIDER
    // ===============================
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL missing in .env");

    let provider = Arc::new(Provider::<Http>::try_from(&rpc_url)?);

    // ===============================
    // WALLET SIGNER (EOA) - READ FROM .ENV
    // ===============================
    let private_key = std::env::var("PRIVATE_KEY").expect("PRIVATE_KEY missing in .env file");

    let proxy_wallet = std::env::var("PROXY_WALLET").expect("PROXY_WALLET missing in .env file");

    let signer = WalletSigner::new(&private_key, 137)?;

    info!("🔑 Signer loaded");
    info!("🧾 Proxy wallet: {}", proxy_wallet);

    // ===============================
    // STAGE 2 — WALLET / ALLOWANCE PREFLIGHT
    // ===============================
    let execution_mode = std::env::var("EXECUTION_MODE").unwrap_or_else(|_| "executor".into());

    if execution_mode == "direct" {
        verify_allowances(provider.clone(), &proxy_wallet).await?;
        info!("✅ STAGE 2 COMPLETE — wallet, allowance, approvals verified");
    } else {
        info!("ℹ️ EXECUTION_MODE=executor, skipping direct allowance preflight in Rust core");
    }

    // ===============================
    // API CREDENTIALS (Load before CLOB Client)
    // ===============================
    let api_key = std::env::var("POLY_API_KEY").expect("POLY_API_KEY missing in .env file");
    let api_secret =
        std::env::var("POLY_API_SECRET").expect("POLY_API_SECRET missing in .env file");
    let api_passphrase =
        std::env::var("POLY_API_PASSPHRASE").expect("POLY_API_PASSPHRASE missing in .env file");

    let read_only = std::env::var("READ_ONLY")
        .unwrap_or_else(|_| "true".to_string())
        .parse::<bool>()
        .unwrap_or(true);

    // ===============================
    // CLOB CLIENT (Now with API credentials)
    // ===============================
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

    let executor = if execution_mode == "executor" {
        let url =
            std::env::var("EXECUTOR_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".to_string());
        let client = ExecutorClient::new(url)?;
        client.healthcheck().await?;
        Some(client)
    } else {
        None
    };

    // ===============================
    // API CLIENT
    // ===============================
    let api = Arc::new(PolymarketClient::new(
        config.polymarket.gamma_api_url.clone(),
        config.polymarket.clob_api_url.clone(),
        api_key,
        api_secret,
        api_passphrase,
        read_only,
        clob.clone(),
    ));

    // ===============================
    // CORE OBJECTS
    // ===============================
    let _price_cache = PriceCache::new();

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

    let mut current_period = current_15m_period();

    // ===============================
    // MAIN LOOP
    // ===============================
    loop {
        let pair_cfg = selected_pair_config()?;
        info!(
            "🔍 Discovering current 15m markets for configured pair: {}-{}",
            pair_cfg.left_name, pair_cfg.right_name
        );

        let (left_market, right_market) = discover_markets(&api, pair_cfg).await?;

        info!("✅ {} Market: {}", pair_cfg.left_name, left_market.slug);
        info!("✅ {} Market: {}", pair_cfg.right_name, right_market.slug);

        let monitor = MarketMonitor::new(
            api.clone(),
            left_market,
            right_market,
            pair_cfg.left_name.to_string(),
            pair_cfg.right_name.to_string(),
            config.trading.check_interval_ms,
        );

        // ╔═══════════════════════════════════════════════════════════╗
        // ║  CHANGED SECTION - Lines 166-199                         ║
        // ║  What: Fixed error handling and added debug logging      ║
        // ║  Why: Silent failures prevented seeing trader errors     ║
        // ╚═══════════════════════════════════════════════════════════╝
        let monitor_handle = tokio::spawn({
            let detector = detector.clone();
            let trader = trader.clone();

            async move {
                monitor
                    .start_monitoring(move |snapshot| {
                        let detector = detector.clone();
                        let trader = trader.clone();

                        async move {
                            // CHANGED: Store opportunities instead of inline iteration
                            let opportunities = detector.detect_opportunities(&snapshot);

                            // CHANGED: Log how many opportunities found
                            if !opportunities.is_empty() {
                                info!(
                                    "🔔 Found {} arbitrage opportunity(ies)!",
                                    opportunities.len()
                                );
                            }

                            // CHANGED: Explicit enumeration with proper error handling
                            for (i, o) in opportunities.iter().enumerate() {
                                info!(
                                    "📋 Processing opportunity {} of {}",
                                    i + 1,
                                    opportunities.len()
                                );

                                // CHANGED: Use match instead of let _ to catch errors
                                match trader.execute_arbitrage(&o).await {
                                    Ok(_) => {
                                        info!("✅ Opportunity {} handled successfully", i + 1);
                                    }
                                    Err(e) => {
                                        warn!("❌ Opportunity {} failed: {}", i + 1, e);
                                    }
                                }
                            }
                        }
                    })
                    .await;
            }
        });
        // ╔═══════════════════════════════════════════════════════════╗
        // ║  END OF CHANGED SECTION                                   ║
        // ╚═══════════════════════════════════════════════════════════╝

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let new_period = current_15m_period();

            if new_period != current_period {
                info!("⏰ 15m rollover — restarting monitor");
                current_period = new_period;
                monitor_handle.abort();
                break;
            }
        }
    }
}

// ===============================
// MARKET DISCOVERY (OUTSIDE MAIN)
// ===============================
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
