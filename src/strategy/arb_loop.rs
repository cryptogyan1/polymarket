// src/strategy/arb_loop.rs

use anyhow::Result;
use chrono::Utc;
use tracing::info;

use crate::config::Config;
use crate::execution::atomic::execute_atomic_arb;
use crate::market::orderbook::fetch_orderbook;
use crate::strategy::arbitrage::detect_arbitrage;
use crate::market::discovery::DiscoveredMarkets;

fn time_remaining(end: chrono::DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = end - now;

    if diff.num_seconds() <= 0 {
        return "CLOSED".to_string();
    }

    let mins = diff.num_minutes();
    let secs = diff.num_seconds() % 60;

    format!("{:02}m {:02}s", mins, secs)
}

pub async fn arb_loop(
    discovered: DiscoveredMarkets,
    config: Config,
) -> Result<()> {
    loop {
        let btc_book = fetch_orderbook(&discovered.btc_yes_token).await?;
        let eth_book = fetch_orderbook(&discovered.eth_no_token).await?;

        let btc_best = match btc_book.best_ask() {
            Some(v) => v,
            None => continue,
        };

        let eth_best = match eth_book.best_ask() {
            Some(v) => v,
            None => continue,
        };

        let arb = detect_arbitrage(
            &crate::market::orderbook::OrderBookTop {
                yes_ask: btc_best,
                no_ask: eth_best,
            },
            config.max_balance,
            config.fee_pct,
            config.min_roi,
        );

        if let Some(op) = arb {
            println!();
            println!("📊 ARBITRAGE MARKET CONTEXT");
            println!("──────────────────────────");

            println!("🟢 {}", discovered.btc_market_title);
            println!(
                "🔗 https://polymarket.com/market/{}",
                discovered.btc_market_slug
            );
            println!(
                "⏱ Closes in: {}",
                time_remaining(discovered.btc_end_time)
            );

            println!();

            println!("🔴 {}", discovered.eth_market_title);
            println!(
                "🔗 https://polymarket.com/market/{}",
                discovered.eth_market_slug
            );
            println!(
                "⏱ Closes in: {}",
                time_remaining(discovered.eth_end_time)
            );

            println!("──────────────────────────");

            info!(
                profit = op.net_profit,
                roi = op.roi,
                yes_price = op.yes_price,
                no_price = op.no_price,
                shares = op.max_shares,
                "Arbitrage opportunity detected"
            );

            if !config.simulation {
                execute_atomic_arb(
                    &discovered.btc_yes_token,
                    &discovered.eth_no_token,
                    op.yes_price,
                    op.no_price,
                    op.max_shares as f64,
                )
                .await?;
            } else {
                info!("SIMULATION MODE — trade not sent");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
