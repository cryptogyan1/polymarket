use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BookLevel {
    price: f64,
    size: f64,
}

#[derive(Debug, Deserialize)]
struct OrderBook {
    asks: Vec<BookLevel>,
    bids: Vec<BookLevel>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let token_id = std::env::args()
        .nth(1)
        .expect("Usage: cargo run --bin token_diagnostic <TOKEN_ID>");

    let clob = "https://clob.polymarket.com";
    let client = reqwest::Client::new();

    println!("\n=== TOKEN ID DIAGNOSTIC ===");
    println!("Token ID (as provided): {}", token_id);
    println!("Token ID length: {} characters\n", token_id.len());

    // Try fetching the orderbook
    let ob_url = format!("{}/orderbook/{}", clob, token_id);
    println!("Attempting to fetch: {}\n", ob_url);

    match client.get(&ob_url).send().await {
        Ok(response) => {
            let status = response.status();
            println!("HTTP Status: {}", status);

            if status.is_success() {
                let body = response.text().await?;
                println!("Response body preview (first 500 chars):");
                println!("{}\n", &body[..body.len().min(500)]);

                // Try to parse it
                match serde_json::from_str::<OrderBook>(&body) {
                    Ok(ob) => {
                        println!("✅ Successfully parsed orderbook!");
                        println!("   Asks: {} levels", ob.asks.len());
                        println!("   Bids: {} levels", ob.bids.len());

                        if let Some(ask) = ob.asks.first() {
                            println!("   Best Ask: ${:.4} (size: {})", ask.price, ask.size);
                        }
                        if let Some(bid) = ob.bids.first() {
                            println!("   Best Bid: ${:.4} (size: {})", bid.price, bid.size);
                        }
                    }
                    Err(e) => {
                        println!("❌ Failed to parse orderbook: {}", e);
                        println!("This might be an API error or the market is inactive.");
                    }
                }
            } else {
                println!("❌ API returned error status");
                let body = response.text().await.unwrap_or_default();
                println!("Error response: {}", body);
            }
        }
        Err(e) => {
            println!("❌ Network error: {}", e);
        }
    }

    // Try converting to hex if it's decimal
    if token_id.chars().all(|c| c.is_ascii_digit()) {
        println!("\n--- Token ID appears to be decimal ---");
        if let Ok(num) = token_id.parse::<u128>() {
            let hex = format!("{:x}", num);
            println!("Hex equivalent: 0x{}", hex);
            println!(
                "Try running with: cargo run --bin token_diagnostic 0x{}",
                hex
            );
        }
    }

    // Try removing 0x prefix if present
    if token_id.starts_with("0x") {
        println!("\n--- Token ID has 0x prefix ---");
        let without_prefix = &token_id[2..];
        println!("Without prefix: {}", without_prefix);
        println!(
            "Try running with: cargo run --bin token_diagnostic {}",
            without_prefix
        );
    }

    println!("\n=== SUGGESTIONS ===");
    println!("1. Make sure the market is currently active");
    println!("2. Token IDs from the API might be in different formats");
    println!("3. Try running the price_monitor to see active markets");
    println!("4. Check if you need to use hex (0x...) or decimal format");
    println!("========================\n");

    Ok(())
}
