use anyhow::{anyhow, Result};
use serde::Deserialize;
use tokio::try_join;

use crate::client::PolymarketClient;

#[derive(Debug, Clone)]
pub struct OrderBook {
    pub bids: Vec<(f64, f64)>, // (price, size)
    pub asks: Vec<(f64, f64)>,
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids.first().cloned()
    }

    pub fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks.first().cloned()
    }

    /// Returns the cheapest ask level that can fill at least `min_size` shares.
    pub fn cheapest_ask_with_min_size(&self, min_size: f64) -> Option<(f64, f64)> {
        self.asks
            .iter()
            .filter(|(_, size)| *size >= min_size)
            .min_by(|(price_a, _), (price_b, _)| price_a.total_cmp(price_b))
            .cloned()
    }

    pub fn estimated_buy_cost(&self, target_shares: f64) -> Option<f64> {
        if target_shares <= 0.0 {
            return Some(0.0);
        }

        let mut remaining = target_shares;
        let mut total_cost = 0.0;

        for (price, size) in &self.asks {
            if remaining <= 0.0 {
                break;
            }
            let fill = remaining.min(*size);
            total_cost += fill * *price;
            remaining -= fill;
        }

        if remaining > 0.0 {
            None
        } else {
            Some(total_cost)
        }
    }

    pub fn estimated_avg_buy_price(&self, target_shares: f64) -> Option<f64> {
        if target_shares <= 0.0 {
            return Some(0.0);
        }
        self.estimated_buy_cost(target_shares)
            .map(|cost| cost / target_shares)
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringOrFloat {
    String(String),
    Float(f64),
}

fn parse_string_or_float<'de, D>(deserializer: D) -> std::result::Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    match StringOrFloat::deserialize(deserializer)? {
        StringOrFloat::String(s) => s.parse::<f64>().map_err(D::Error::custom),
        StringOrFloat::Float(f) => Ok(f),
    }
}

#[derive(Debug, Deserialize)]
struct BookLevel {
    #[serde(deserialize_with = "parse_string_or_float")]
    price: f64,
    #[serde(deserialize_with = "parse_string_or_float")]
    size: f64,
}

#[derive(Debug, Deserialize)]
struct BookResponse {
    #[serde(default)]
    bids: Vec<BookLevel>,
    #[serde(default)]
    asks: Vec<BookLevel>,
}

#[derive(Debug, Deserialize)]
struct PriceResponse {
    price: String,
}

pub async fn fetch_orderbook(api: &PolymarketClient, token_id: &str) -> Result<OrderBook> {
    let client = api.http_client();

    let book_url = format!("{}/book?token_id={}", api.clob_url, token_id);
    let book_resp = client.get(&book_url).send().await?;

    if book_resp.status().is_success() {
        let book_data: BookResponse = book_resp.json().await?;

        let mut bids = book_data
            .bids
            .into_iter()
            .map(|b| (b.price, b.size))
            .collect::<Vec<_>>();
        let mut asks = book_data
            .asks
            .into_iter()
            .map(|a| (a.price, a.size))
            .collect::<Vec<_>>();

        // Normalize order so best bid/ask are always first.
        bids.sort_by(|(price_a, _), (price_b, _)| price_b.total_cmp(price_a));
        asks.sort_by(|(price_a, _), (price_b, _)| price_a.total_cmp(price_b));

        if !bids.is_empty() || !asks.is_empty() {
            return Ok(OrderBook { bids, asks });
        }
    }

    // Fallback to /price endpoint when /book is unavailable.
    let ask_url = format!("{}/price?token_id={}&side=SELL", api.clob_url, token_id);
    let bid_url = format!("{}/price?token_id={}&side=BUY", api.clob_url, token_id);

    let (bid_response, ask_response) =
        try_join!(client.get(&bid_url).send(), client.get(&ask_url).send(),)?;

    if !bid_response.status().is_success() {
        return Err(anyhow!(
            "Failed to fetch bid price: {}",
            bid_response.status()
        ));
    }
    if !ask_response.status().is_success() {
        return Err(anyhow!(
            "Failed to fetch ask price: {}",
            ask_response.status()
        ));
    }

    let bid_data: PriceResponse = bid_response.json().await?;
    let bid_price: f64 = bid_data
        .price
        .parse()
        .map_err(|e| anyhow!("Failed to parse bid price: {}", e))?;

    let ask_data: PriceResponse = ask_response.json().await?;
    let ask_price: f64 = ask_data
        .price
        .parse()
        .map_err(|e| anyhow!("Failed to parse ask price: {}", e))?;

    Ok(OrderBook {
        bids: vec![(bid_price, 0.0)],
        asks: vec![(ask_price, 0.0)],
    })
}
