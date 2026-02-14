use polymarket_15m_arbitrage_bot::*;

use anyhow::Result;
use client::PolymarketClient;
use execution::clob_client::ClobClient;
use execution::orderbook::fetch_orderbook;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment
    dotenv::dotenv().ok();

    // Get config
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL missing");
    let private_key = std::env::var("PRIVATE_KEY").expect("PRIVATE_KEY missing");
    let proxy_wallet = std::env::var("PROXY_WALLET").expect("PROXY_WALLET missing");
    let api_key = std::env::var("POLY_API_KEY").expect("POLY_API_KEY missing");
    let api_secret = std::env::var("POLY_API_SECRET").expect("POLY_API_SECRET missing");
    let api_passphrase = std::env::var("POLY_API_PASSPHRASE").expect("POLY_API_PASSPHRASE missing");

    // Initialize CLOB client
    let clob = Arc::new(
        ClobClient::new(
            rpc_url.as_str(),
            private_key.as_str(),
            proxy_wallet.as_str(),
            String::new(),
            String::new(),
            String::new(),
        )
        .await?,
    );

    // Initialize API client
    let api = Arc::new(PolymarketClient::new(
        "https://gamma-api.polymarket.com".to_string(),
        "https://clob.polymarket.com".to_string(),
        api_key,
        api_secret,
        api_passphrase,
        true, // read only
        Some(clob),
    ));

    // Clear screen
    print!("\x1B[2J\x1B[1;1H");

    println!("🔍 Discovering markets...\n");

    // Discover current markets
    let (eth_market, btc_market) = discover_markets(&api).await?;

    println!("✅ Found ETH Market: {}", eth_market.slug);
    println!("✅ Found BTC Market: {}\n", btc_market.slug);

    // Get token IDs from clob_token_ids field (JSON array string)
    let eth_token_ids_str = eth_market
        .clob_token_ids
        .as_ref()
        .expect("ETH clob_token_ids missing");
    let btc_token_ids_str = btc_market
        .clob_token_ids
        .as_ref()
        .expect("BTC clob_token_ids missing");

    // Parse as JSON array
    let eth_token_ids: Vec<String> =
        serde_json::from_str(eth_token_ids_str).expect("Failed to parse ETH token IDs as JSON");
    let btc_token_ids: Vec<String> =
        serde_json::from_str(btc_token_ids_str).expect("Failed to parse BTC token IDs as JSON");

    if eth_token_ids.len() < 2 || btc_token_ids.len() < 2 {
        panic!("Expected 2 tokens per market (UP and DOWN)");
    }

    // First token is typically UP (Yes), second is DOWN (No)
    let eth_up = &eth_token_ids[0];
    let eth_down = &eth_token_ids[1];
    let btc_up = &btc_token_ids[0];
    let btc_down = &btc_token_ids[1];

    println!("\nToken Mapping:");
    println!("  ETH UP:   {}", eth_up);
    println!("  ETH DOWN: {}", eth_down);
    println!("  BTC UP:   {}", btc_up);
    println!("  BTC DOWN: {}", btc_down);

    println!("\nPress Ctrl+C to exit\n");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Main display loop
    loop {
        // Fetch all orderbooks
        let eth_up_book = fetch_orderbook(&api, eth_up).await.ok();
        let eth_down_book = fetch_orderbook(&api, eth_down).await.ok();
        let btc_up_book = fetch_orderbook(&api, btc_up).await.ok();
        let btc_down_book = fetch_orderbook(&api, btc_down).await.ok();

        // Clear screen and move cursor to top
        print!("\x1B[2J\x1B[1;1H");
        io::stdout().flush().unwrap();

        // Get current time
        let now = chrono::Local::now();

        // Print the exact box format requested
        println!("======================================================");
        println!("LIVE PRICE MONITOR - {}", now.format("%H:%M:%S"));
        println!("======================================================");
        println!("TOKEN |      UP                    |         DOWN");
        println!("======================================================");

        // ETH Row
        print!("ETH   | ");
        print_prices(&eth_up_book);
        print!(" | ");
        print_prices(&eth_down_book);
        println!();

        println!("======================================================");

        // BTC Row
        print!("BTC   | ");
        print_prices(&btc_up_book);
        print!(" | ");
        print_prices(&btc_down_book);
        println!();

        println!("======================================================");

        // Show arbitrage opportunity if available
        if let (Some(eth_up_ob), Some(btc_down_ob)) = (&eth_up_book, &btc_down_book) {
            if let (Some((eth_ask, _)), Some((btc_bid, _))) =
                (eth_up_ob.best_ask(), btc_down_ob.best_bid())
            {
                let total_cost = eth_ask + btc_bid;
                let potential_profit = 2.0 - total_cost;
                let profit_pct = (potential_profit / total_cost) * 100.0;

                println!();
                if profit_pct > 0.0 {
                    println!("🟢 ARBITRAGE OPPORTUNITY!");
                    println!(
                        "   ETH-UP Ask: ${:.4} + BTC-DOWN Bid: ${:.4}",
                        eth_ask, btc_bid
                    );
                    println!(
                        "   Total Cost: ${:.4} | Profit: ${:.4} ({:.2}%)",
                        total_cost, potential_profit, profit_pct
                    );
                }
            }
        }

        println!("\n🔄 Auto-updating every 2 seconds... (Ctrl+C to exit)");

        // Wait before next update
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn print_prices(book: &Option<execution::orderbook::OrderBook>) {
    match book {
        Some(ob) => {
            let ask = ob
                .best_ask()
                .map(|(p, _)| format!("{:.4}", p))
                .unwrap_or_else(|| "N/A".to_string());
            let bid = ob
                .best_bid()
                .map(|(p, _)| format!("{:.4}", p))
                .unwrap_or_else(|| "N/A".to_string());

            print!("ASK-{:<8} BID-{:<8}", ask, bid);
        }
        None => {
            print!("ASK-N/A      BID-N/A     ");
        }
    }
}

// Market discovery (same as main bot)
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
                println!("Found {} market: {}", name, market.slug);
                return Ok(market);
            }
        }
    }

    anyhow::bail!("No active {} market found", name)
}
