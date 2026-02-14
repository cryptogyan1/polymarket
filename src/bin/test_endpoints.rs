use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PriceResponse {
    #[serde(deserialize_with = "deserialize_string_to_f64_option")]
    price: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct BookLevel {
    #[serde(deserialize_with = "deserialize_string_to_f64")]
    price: f64,
    #[serde(deserialize_with = "deserialize_string_to_f64")]
    size: f64,
}

#[derive(Debug, Deserialize)]
struct OrderBook {
    #[serde(default)]
    asks: Vec<BookLevel>,
    #[serde(default)]
    bids: Vec<BookLevel>,
}

// Helper to parse string to f64
fn deserialize_string_to_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrFloat {
        String(String),
        Float(f64),
    }

    match StringOrFloat::deserialize(deserializer)? {
        StringOrFloat::String(s) => s.parse::<f64>().map_err(serde::de::Error::custom),
        StringOrFloat::Float(f) => Ok(f),
    }
}

// Helper to parse optional string to f64
fn deserialize_string_to_f64_option<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrFloat {
        String(String),
        Float(f64),
    }

    let opt = Option::<StringOrFloat>::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(StringOrFloat::String(s)) => {
            s.parse::<f64>().map(Some).map_err(serde::de::Error::custom)
        }
        Some(StringOrFloat::Float(f)) => Ok(Some(f)),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let token_id = std::env::args()
        .nth(1)
        .expect("Usage: cargo run --bin test_endpoints <TOKEN_ID>");

    let clob = "https://clob.polymarket.com";
    let client = reqwest::Client::new();

    println!("\n=== TESTING ALL API ENDPOINTS ===");
    println!("Token ID: {}\n", token_id);

    // Test 1: /book endpoint
    println!("📍 Testing: /book?token_id=...");
    let book_url = format!("{}/book?token_id={}", clob, token_id);
    println!("   URL: {}", book_url);

    match client.get(&book_url).send().await {
        Ok(response) => {
            let status = response.status();
            println!("   Status: {}", status);

            if status.is_success() {
                let text = response.text().await?;
                match serde_json::from_str::<OrderBook>(&text) {
                    Ok(book) => {
                        println!("   ✅ SUCCESS!");
                        println!("   Bids: {} levels", book.bids.len());
                        println!("   Asks: {} levels", book.asks.len());
                        if let Some(bid) = book.bids.first() {
                            println!("   Best Bid: ${:.4} (size: {})", bid.price, bid.size);
                        }
                        if let Some(ask) = book.asks.first() {
                            println!("   Best Ask: ${:.4} (size: {})", ask.price, ask.size);
                        }
                    }
                    Err(e) => {
                        println!("   ❌ JSON Parse Error: {}", e);
                        println!("   Response: {}", &text[..text.len().min(500)]);
                    }
                }
            } else {
                println!("   ❌ Failed: HTTP {}", status);
            }
        }
        Err(e) => {
            println!("   ❌ Network Error: {}", e);
        }
    }

    println!();

    // Test 2: /price with side=BUY (best bid)
    println!("📍 Testing: /price?token_id=...&side=BUY");
    let bid_url = format!("{}/price?token_id={}&side=BUY", clob, token_id);
    println!("   URL: {}", bid_url);

    match client.get(&bid_url).send().await {
        Ok(response) => {
            let status = response.status();
            println!("   Status: {}", status);

            if status.is_success() {
                match response.json::<PriceResponse>().await {
                    Ok(data) => {
                        if let Some(price) = data.price {
                            println!("   ✅ Best Bid: ${:.4}", price);
                        } else {
                            println!("   ⚠️  No bid price available");
                        }
                    }
                    Err(e) => {
                        println!("   ❌ JSON Error: {}", e);
                    }
                }
            } else {
                println!("   ❌ Failed: HTTP {}", status);
            }
        }
        Err(e) => {
            println!("   ❌ Network Error: {}", e);
        }
    }

    println!();

    // Test 3: /price with side=SELL (best ask)
    println!("📍 Testing: /price?token_id=...&side=SELL");
    let ask_url = format!("{}/price?token_id={}&side=SELL", clob, token_id);
    println!("   URL: {}", ask_url);

    match client.get(&ask_url).send().await {
        Ok(response) => {
            let status = response.status();
            println!("   Status: {}", status);

            if status.is_success() {
                match response.json::<PriceResponse>().await {
                    Ok(data) => {
                        if let Some(price) = data.price {
                            println!("   ✅ Best Ask: ${:.4}", price);
                        } else {
                            println!("   ⚠️  No ask price available");
                        }
                    }
                    Err(e) => {
                        println!("   ❌ JSON Error: {}", e);
                    }
                }
            } else {
                println!("   ❌ Failed: HTTP {}", status);
            }
        }
        Err(e) => {
            println!("   ❌ Network Error: {}", e);
        }
    }

    println!();

    // Test 4: /orderbook (old endpoint - probably broken)
    println!("📍 Testing: /orderbook/... (old endpoint)");
    let old_url = format!("{}/orderbook/{}", clob, token_id);
    println!("   URL: {}", old_url);

    match client.get(&old_url).send().await {
        Ok(response) => {
            let status = response.status();
            println!("   Status: {}", status);
            println!("   ⚠️  This endpoint is likely deprecated");
        }
        Err(e) => {
            println!("   ❌ Network Error: {}", e);
        }
    }

    println!();
    println!("=== SUMMARY ===");
    println!("If /book or /price endpoints work, your bot can fetch prices!");
    println!("If all fail, the market is probably inactive (markets only run 15 mins)");
    println!();

    Ok(())
}
