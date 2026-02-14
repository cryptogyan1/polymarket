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
        info!("🔍 Discovering current 15m markets...");

        let (eth_market, btc_market) = discover_markets(&api).await?;

        info!("✅ ETH Market: {}", eth_market.slug);
        info!("✅ BTC Market: {}", btc_market.slug);

        let monitor = MarketMonitor::new(
            api.clone(),
            eth_market,
            btc_market,
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
async fn discover_markets(api: &PolymarketClient) -> Result<(domain::Market, domain::Market)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    let mut seen = std::collections::HashSet::new();

    let eth = discover_market(api, "ETH", "eth", now, &mut seen).await?;
    seen.insert(eth.condition_id.clone());

    let btc = discover_market(api, "BTC", "btc", now, &mut seen).await?;

    Ok((eth, btc))
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
