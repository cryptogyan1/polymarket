use anyhow::{Context, Result};
use futures_util::{stream, StreamExt};
use polymarket_15m_arbitrage_bot::{
    client::PolymarketClient,
    domain::order::Side,
    execution::{orderbook::fetch_orderbook, ExecutorClient},
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
struct SportsMarket {
    slug: String,
    condition_id: String,
    question: String,
    outcome_a_label: String,
    outcome_a_token: String,
    outcome_b_label: String,
    outcome_b_token: String,
    sports_market_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SportsMarketTypesResponse {
    #[serde(rename = "marketTypes", default)]
    market_types: Vec<String>,
}

#[derive(Debug)]
struct Opportunity {
    market: SportsMarket,
    outcome_a_ask: f64,
    outcome_b_ask: f64,
    sum: f64,
    edge: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    let gamma_url = std::env::var("GAMMA_API_URL")
        .unwrap_or_else(|_| "https://gamma-api.polymarket.com".to_string());
    let clob_url = std::env::var("CLOB_API_URL")
        .unwrap_or_else(|_| "https://clob.polymarket.com".to_string());

    let api = Arc::new(PolymarketClient::new(
        gamma_url.clone(),
        clob_url,
        std::env::var("POLY_API_KEY").unwrap_or_default(),
        std::env::var("POLY_API_SECRET").unwrap_or_default(),
        std::env::var("POLY_API_PASSPHRASE").unwrap_or_default(),
        true,
        None,
    ));

    let max_sum = env_f64("ARBITRAGE_MAX_SUM", 0.985);
    let min_size = env_f64("MIN_SHARES", 5.0);
    let scan_interval = Duration::from_millis(env_u64("SPORTS_SCAN_INTERVAL_MS", 250));
    let max_pages = env_u64("SPORTS_MAX_DISCOVERY_PAGES", 20) as usize;
    let page_concurrency = env_u64("SPORTS_DISCOVERY_CONCURRENCY", 8) as usize;
    let scan_concurrency = env_u64("SPORTS_SCAN_CONCURRENCY", 128) as usize;
    let auto_trade = env_bool("SPORTS_AUTO_TRADE", false);
    let size_usdc = env_f64("SPORTS_TRADE_SIZE_USDC", 5.0);
    let cooldown = Duration::from_millis(env_u64("OPPORTUNITY_COOLDOWN_MS", 5000));

    let executor = if auto_trade {
        let url =
            std::env::var("EXECUTOR_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".to_string());
        Some(ExecutorClient::new(url)?)
    } else {
        None
    };

    println!("🏈 Discovering active sports markets in parallel...");
    let markets = discover_sports_markets(&gamma_url, max_pages, page_concurrency)
        .await
        .context("failed to discover sports markets")?;

    println!(
        "✅ Loaded {} active 2-outcome sports markets | scan interval={}ms",
        markets.len(),
        scan_interval.as_millis()
    );

    let mut last_seen: HashMap<String, Instant> = HashMap::new();

    loop {
        let opportunities = scan_once(api.clone(), &markets, max_sum, min_size, scan_concurrency).await;

        for opp in opportunities {
            let key = opp.market.slug.clone();
            if let Some(ts) = last_seen.get(&key) {
                if ts.elapsed() < cooldown {
                    continue;
                }
            }
            last_seen.insert(key, Instant::now());

            println!(
                "⚡ arb {} [{}] {} | {}={:.4} {}={:.4} SUM={:.4} EDGE={:.4}",
                opp.market.slug,
                opp.market
                    .sports_market_type
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                opp.market.question,
                opp.market.outcome_a_label,
                opp.outcome_a_ask,
                opp.market.outcome_b_label,
                opp.outcome_b_ask,
                opp.sum,
                opp.edge
            );

            if let Some(exec) = &executor {
                let (a_res, b_res) = tokio::join!(
                    exec.execute_order(
                        &opp.market.outcome_a_token,
                        Side::Buy,
                        opp.outcome_a_ask,
                        size_usdc
                    ),
                    exec.execute_order(
                        &opp.market.outcome_b_token,
                        Side::Buy,
                        opp.outcome_b_ask,
                        size_usdc
                    )
                );

                match (a_res, b_res) {
                    (Ok(a), Ok(b)) => {
                        println!(
                            "✅ placed both legs market={} {}={:?} {}={:?}",
                            opp.market.slug,
                            opp.market.outcome_a_label,
                            a.order_id,
                            opp.market.outcome_b_label,
                            b.order_id
                        );
                    }
                    (a, b) => {
                        eprintln!(
                            "❌ execution failed for {} | {}={:?} {}={:?}",
                            opp.market.slug,
                            opp.market.outcome_a_label,
                            a,
                            opp.market.outcome_b_label,
                            b
                        );
                    }
                }
            }
        }

        tokio::time::sleep(scan_interval).await;
    }
}

async fn scan_once(
    api: Arc<PolymarketClient>,
    markets: &[SportsMarket],
    max_sum: f64,
    min_size: f64,
    concurrency: usize,
) -> Vec<Opportunity> {
    stream::iter(markets.iter().cloned())
        .map(|market| {
            let api = api.clone();
            async move {
                let (a_book, b_book) = tokio::join!(
                    fetch_orderbook(&api, &market.outcome_a_token),
                    fetch_orderbook(&api, &market.outcome_b_token)
                );

                let a_book = a_book.ok()?;
                let b_book = b_book.ok()?;

                let (a_ask, a_size) = a_book.cheapest_ask_with_min_size(min_size)?;
                let (b_ask, b_size) = b_book.cheapest_ask_with_min_size(min_size)?;
                if a_size < min_size || b_size < min_size {
                    return None;
                }

                let sum = a_ask + b_ask;
                if sum >= max_sum {
                    return None;
                }

                Some(Opportunity {
                    market,
                    outcome_a_ask: a_ask,
                    outcome_b_ask: b_ask,
                    sum,
                    edge: 1.0 - sum,
                })
            }
        })
        .buffer_unordered(concurrency)
        .filter_map(async move |x| x)
        .collect()
        .await
}

async fn discover_sports_markets(
    gamma_url: &str,
    max_pages: usize,
    concurrency: usize,
) -> Result<Vec<SportsMarket>> {
    let http = Client::new();
    let market_types = fetch_market_types(&http, gamma_url).await.unwrap_or_default();

    let offsets: Vec<usize> = (0..max_pages).map(|i| i * 100).collect();

    let pages: Vec<Vec<SportsMarket>> = stream::iter(offsets)
        .map(|offset| {
            let http = http.clone();
            let base = gamma_url.to_string();
            let market_types = market_types.clone();
            async move {
                fetch_page_markets(&http, &base, offset, &market_types)
                    .await
                    .unwrap_or_default()
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for market in pages.into_iter().flatten() {
        if seen.insert(market.condition_id.clone()) {
            out.push(market);
        }
    }
    Ok(out)
}

async fn fetch_market_types(http: &Client, gamma_url: &str) -> Result<HashSet<String>> {
    let url = format!("{}/sports/market-types", gamma_url.trim_end_matches('/'));
    let resp = http.get(url).send().await?.error_for_status()?;
    let body: SportsMarketTypesResponse = resp.json().await?;
    Ok(body
        .market_types
        .into_iter()
        .map(|s| s.to_lowercase())
        .collect())
}

async fn fetch_page_markets(
    http: &Client,
    gamma_url: &str,
    offset: usize,
    market_types: &HashSet<String>,
) -> Result<Vec<SportsMarket>> {
    let url = format!(
        "{}/events?active=true&closed=false&limit=100&offset={}",
        gamma_url.trim_end_matches('/'),
        offset
    );

    let events: Vec<Value> = http.get(url).send().await?.error_for_status()?.json().await?;
    let mut markets = Vec::new();

    for event in events {
        let event_markets = event
            .get("markets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for m in event_markets {
            if let Some(parsed) = parse_sports_market(&m, market_types) {
                markets.push(parsed);
            }
        }
    }

    Ok(markets)
}

fn parse_sports_market(v: &Value, market_types: &HashSet<String>) -> Option<SportsMarket> {
    if !v.get("active")?.as_bool()? || v.get("closed")?.as_bool()? {
        return None;
    }

    let sports_type = v
        .get("sportsMarketType")
        .or_else(|| v.get("sports_market_type"))
        .or_else(|| v.get("marketType"))
        .and_then(Value::as_str)
        .map(|s| s.to_lowercase());

    // Keep markets if type is missing (some payload variants omit it),
    // but when present and we have a known valid-type set, enforce it.
    if !market_types.is_empty() {
        if let Some(t) = sports_type.as_ref() {
            if !market_types.contains(t) {
                return None;
            }
        }
    }

    let condition_id = v.get("conditionId")?.as_str()?.to_string();
    let slug = v.get("slug")?.as_str()?.to_string();
    let question = v
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let (outcome_a_label, outcome_a_token, outcome_b_label, outcome_b_token) =
        extract_binary_outcomes(v)?;

    Some(SportsMarket {
        slug,
        condition_id,
        question,
        outcome_a_label,
        outcome_a_token,
        outcome_b_label,
        outcome_b_token,
        sports_market_type: sports_type,
    })
}

fn extract_binary_outcomes(v: &Value) -> Option<(String, String, String, String)> {
    if let Some(tokens) = v.get("tokens").and_then(Value::as_array) {
        let parsed = tokens
            .iter()
            .filter_map(|t| {
                let label = t.get("outcome")?.as_str()?.to_string();
                let token = t
                    .get("tokenId")
                    .or_else(|| t.get("token_id"))
                    .and_then(Value::as_str)?
                    .to_string();
                Some((label, token))
            })
            .take(2)
            .collect::<Vec<_>>();

        if parsed.len() == 2 {
            return Some((
                parsed[0].0.clone(),
                parsed[0].1.clone(),
                parsed[1].0.clone(),
                parsed[1].1.clone(),
            ));
        }
    }

    let outcomes = v
        .get("outcomes")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())?;

    let token_ids = v
        .get("clobTokenIds")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())?;

    if outcomes.len() >= 2 && token_ids.len() >= 2 {
        return Some((
            outcomes[0].clone(),
            token_ids[0].clone(),
            outcomes[1].clone(),
            token_ids[1].clone(),
        ));
    }

    None
}

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(default)
}
