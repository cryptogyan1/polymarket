// src/market/discovery.rs

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct GammaMarket {
    pub id: String,

    #[serde(default)]
    pub title: Option<String>,

    pub slug: String,

    #[serde(rename = "endTime")]
    pub end_time: String,

    #[serde(default, rename = "outcomeTokens")]
    pub outcome_tokens: Vec<OutcomeToken>,

    #[serde(rename = "clobVerifierContract")]
    pub clob_verifier_contract: String,
}

#[derive(Debug, Deserialize)]
pub struct OutcomeToken {
    pub outcome: String, // "YES" or "NO"

    #[serde(rename = "tokenId")]
    pub token_id: String,
}

#[derive(Debug)]
pub struct DiscoveredMarkets {
    pub btc_yes_token: String,
    pub eth_no_token: String,

    pub btc_market_title: String,
    pub eth_market_title: String,

    pub btc_market_slug: String,
    pub eth_market_slug: String,

    pub btc_end_time: DateTime<Utc>,
    pub eth_end_time: DateTime<Utc>,

    pub verifying_contract: String,
}

pub async fn discover_btc_eth_15m() -> Result<DiscoveredMarkets> {
    let base = std::env::var("GAMMA_API_URL")
        .map_err(|_| anyhow!("GAMMA_API_URL not set"))?;

    let url = format!("{}/markets", base);

    let markets: Vec<GammaMarket> = reqwest::Client::new()
        .get(url)
        .send()
        .await?
        .json()
        .await?;

    let mut btc_market: Option<GammaMarket> = None;
    let mut eth_market: Option<GammaMarket> = None;

    for m in markets {
        if m.outcome_tokens.is_empty() {
            continue;
        }

        let title = m.title.clone().unwrap_or_default().to_lowercase();

        if title.contains("btc") && title.contains("15") {
            btc_market = Some(m);
        } else if title.contains("eth") && title.contains("15") {
            eth_market = Some(m);
        }
    }

    let btc = btc_market.ok_or_else(|| anyhow!("BTC 15m market not found"))?;
    let eth = eth_market.ok_or_else(|| anyhow!("ETH 15m market not found"))?;

    let btc_yes = btc
        .outcome_tokens
        .iter()
        .find(|t| t.outcome == "YES")
        .ok_or_else(|| anyhow!("BTC YES token not found"))?
        .token_id
        .clone();

    let eth_no = eth
        .outcome_tokens
        .iter()
        .find(|t| t.outcome == "NO")
        .ok_or_else(|| anyhow!("ETH NO token not found"))?
        .token_id
        .clone();

    Ok(DiscoveredMarkets {
        btc_yes_token: btc_yes,
        eth_no_token: eth_no,

        btc_market_title: btc.title.unwrap_or_else(|| "Bitcoin Up or Down".to_string()),
        eth_market_title: eth.title.unwrap_or_else(|| "Ethereum Up or Down".to_string()),

        btc_market_slug: btc.slug,
        eth_market_slug: eth.slug,

        btc_end_time: btc.end_time.parse::<DateTime<Utc>>()?,
        eth_end_time: eth.end_time.parse::<DateTime<Utc>>()?,

        verifying_contract: btc.clob_verifier_contract,
    })
}

