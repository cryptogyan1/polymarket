use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BookLevel {
    #[serde(deserialize_with = "deserialize_string_to_f64")]
    price: f64,
    #[serde(deserialize_with = "deserialize_string_to_f64")]
    size: f64,
}

#[derive(Debug, Deserialize)]
struct OrderBook {
    asks: Vec<BookLevel>,
    bids: Vec<BookLevel>,
}

#[derive(Debug, Deserialize)]
struct MidpointResp {
    #[serde(deserialize_with = "deserialize_string_to_f64")]
    midpoint: f64,
}

// Helper function to deserialize string numbers to f64
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

#[tokio::main]
async fn main() -> Result<()> {
    // 🔴 PUT ANY REAL TOKEN_ID HERE (YES or NO)
    let token_id = std::env::args()
        .nth(1)
        .expect("Usage: cargo run --bin verify_ask <TOKEN_ID>");

    let clob = std::env::var("POLYMARKET_CLOB_REST")
        .unwrap_or_else(|_| "https://clob.polymarket.com".to_string());

    let client = reqwest::Client::new();

    println!("\n🔍 Fetching orderbook for token: {}", token_id);

    // ---------------------------
    // 1️⃣ Fetch real orderbook
    // ---------------------------
    let ob_url = format!("{}/book?token_id={}", clob, token_id);
    println!("📡 URL: {}", ob_url);

    let response = client.get(&ob_url).send().await?;
    let status = response.status();

    if !status.is_success() {
        eprintln!("❌ API Error: HTTP {}", status);
        let body = response.text().await.unwrap_or_default();
        eprintln!("Response: {}", body);
        anyhow::bail!("Failed to fetch orderbook - market may be inactive or token ID invalid");
    }

    let body = response.text().await?;

    let ob: OrderBook = match serde_json::from_str(&body) {
        Ok(ob) => ob,
        Err(e) => {
            eprintln!("❌ Failed to parse orderbook JSON");
            eprintln!("Error: {}", e);
            eprintln!("Response body: {}", &body[..body.len().min(200)]);
            anyhow::bail!("Invalid JSON response - the market might be inactive");
        }
    };

    let best_ask = ob.asks.first();
    let best_bid = ob.bids.first();

    // ---------------------------
    // 2️⃣ Fetch UI midpoint
    // ---------------------------
    let mid_url = format!("{}/midpoint?token_id={}", clob, token_id);
    let midpoint: Option<MidpointResp> = client.get(&mid_url).send().await?.json().await.ok();

    // ---------------------------
    // 3️⃣ Print proof
    // ---------------------------
    println!("\n=== POLYMARKET PRICE VERIFICATION ===");
    println!("TOKEN_ID: {}\n", token_id);

    match best_ask {
        Some(a) => println!("BEST ASK  (buy)  → price={} size={}", a.price, a.size),
        None => println!("BEST ASK  → none"),
    }

    match best_bid {
        Some(b) => println!("BEST BID  (sell) → price={} size={}", b.price, b.size),
        None => println!("BEST BID  → none"),
    }

    match midpoint {
        Some(m) => println!("MIDPOINT (UI)    → price={}", m.midpoint),
        None => println!("MIDPOINT         → unavailable"),
    }

    println!("\nNOTE:");
    println!("• BEST ASK is the real executable price");
    println!("• MIDPOINT is UI-only and NOT tradable");
    println!("=====================================\n");

    Ok(())
}
