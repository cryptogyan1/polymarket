use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

// ==================================================
// MARKET + TOKENS
// ==================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    #[serde(rename = "conditionId")]
    pub condition_id: String,
    #[serde(rename = "id")]
    pub market_id: Option<String>,
    pub name: Option<String>,
    pub question: String,
    pub slug: String,
    #[serde(rename = "resolutionSource")]
    pub resolution_source: Option<String>,
    #[serde(rename = "endDateISO")]
    pub end_date_iso: Option<String>,
    #[serde(rename = "endDateIso")]
    pub end_date_iso_alt: Option<String>,
    pub active: bool,
    pub closed: bool,
    pub tokens: Option<Vec<Token>>,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>,
    pub outcomes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    #[serde(rename = "tokenId")]
    pub token_id: String,
    pub outcome: String,
    pub price: Option<Decimal>,
}

// ==================================================
// ORDER BOOK
// ==================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub bids: Vec<OrderBookEntry>,
    pub asks: Vec<OrderBookEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookEntry {
    pub price: Decimal,
    pub size: Decimal,
}

// ==================================================
// PRICES
// ==================================================

#[derive(Debug, Clone)]
pub struct TokenPrice {
    pub token_id: String,
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
    pub bid_size: Option<Decimal>,
    pub ask_size: Option<Decimal>,
}

impl TokenPrice {
    /// Safe ask price (what we pay)
    pub fn ask_price(&self) -> Decimal {
        self.ask.unwrap_or(dec!(1))
    }

    /// Safe bid price (what we receive)
    pub fn bid_price(&self) -> Decimal {
        self.bid.unwrap_or(dec!(0))
    }

    /// Mid price (for diagnostics)
    pub fn mid_price(&self) -> Option<Decimal> {
        match (self.bid, self.ask) {
            (Some(b), Some(a)) => Some((a + b) / dec!(2)),
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            _ => None,
        }
    }
}

// ==================================================
// ORDERS
// ==================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub token_id: String,
    pub side: String,
    pub size: String,
    pub price: String,
    #[serde(rename = "type")]
    pub order_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResponse {
    pub order_id: Option<String>,
    pub status: String,
    pub message: Option<String>,
}

// ==================================================
// MARKET SNAPSHOT
// ==================================================

#[derive(Debug, Clone)]
pub struct MarketData {
    pub condition_id: String,
    pub market_name: String,
    pub up_token: Option<TokenPrice>,
    pub down_token: Option<TokenPrice>,
}

// ==================================================
// ARBITRAGE
// ==================================================
pub mod order;
#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    pub pair_label: String,
    pub eth_up_price: Decimal,
    pub btc_down_price: Decimal,
    pub total_cost: Decimal,
    pub effective_total_cost: Decimal,
    pub expected_profit: Decimal,
    pub eth_up_token_id: String,
    pub btc_down_token_id: String,
    pub eth_condition_id: String,
    pub btc_condition_id: String,
    pub max_shares: Decimal,
    pub eth_leg_ask_size: Decimal,
    pub btc_leg_ask_size: Decimal,
    pub eth_up_bid_price: Decimal,
    pub btc_down_bid_price: Decimal,
}

// ==================================================
// TRADE TRACKING
// ==================================================

#[derive(Debug, Clone)]
pub struct PendingTrade {
    pub eth_token_id: String,
    pub btc_token_id: String,
    pub eth_condition_id: String,
    pub btc_condition_id: String,
    pub investment_amount: f64,
    pub units: f64,
    pub timestamp: std::time::Instant,
}

// ==================================================
// MARKET DETAILS (SETTLEMENT)
// ==================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketToken {
    pub outcome: String,
    pub price: Decimal,
    #[serde(rename = "token_id")]
    pub token_id: String,
    pub winner: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketDetails {
    #[serde(rename = "accepting_order_timestamp")]
    pub accepting_order_timestamp: Option<String>,
    #[serde(rename = "accepting_orders")]
    pub accepting_orders: bool,
    pub active: bool,
    pub archived: bool,
    #[serde(rename = "condition_id")]
    pub condition_id: String,
    pub description: String,
    #[serde(rename = "enable_order_book")]
    pub enable_order_book: bool,
    #[serde(rename = "end_date_iso")]
    pub end_date_iso: String,
    pub fpmm: String,
    #[serde(rename = "game_start_time")]
    pub game_start_time: Option<String>,
    pub icon: String,
    pub image: String,
    #[serde(rename = "is_50_50_outcome")]
    pub is_50_50_outcome: bool,
    #[serde(rename = "maker_base_fee")]
    pub maker_base_fee: Decimal,
    #[serde(rename = "market_slug")]
    pub market_slug: String,
    #[serde(rename = "minimum_order_size")]
    pub minimum_order_size: Decimal,
    #[serde(rename = "minimum_tick_size")]
    pub minimum_tick_size: Decimal,
    #[serde(rename = "neg_risk")]
    pub neg_risk: bool,
    #[serde(rename = "neg_risk_market_id")]
    pub neg_risk_market_id: String,
    #[serde(rename = "neg_risk_request_id")]
    pub neg_risk_request_id: String,
    #[serde(rename = "notifications_enabled")]
    pub notifications_enabled: bool,
    pub question: String,
    #[serde(rename = "question_id")]
    pub question_id: String,
    pub rewards: Rewards,
    #[serde(rename = "seconds_delay")]
    pub seconds_delay: u32,
    pub tags: Vec<String>,
    #[serde(rename = "taker_base_fee")]
    pub taker_base_fee: Decimal,
    pub tokens: Vec<MarketToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rewards {
    #[serde(rename = "max_spread")]
    pub max_spread: Decimal,
    #[serde(rename = "min_size")]
    pub min_size: Decimal,
    pub rates: Option<serde_json::Value>,
}

// ==================================================
// ORDER BOOK SIMULATION (OPTIONAL)
// ==================================================

#[derive(Debug, Clone)]
pub struct OrderBookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

pub fn simulate_buy_cost(asks: &[OrderBookLevel], target_size: Decimal) -> Option<Decimal> {
    let mut remaining = target_size;
    let mut cost = dec!(0);

    for level in asks {
        if remaining <= dec!(0) {
            break;
        }
        let fill = remaining.min(level.size);
        cost += fill * level.price;
        remaining -= fill;
    }

    if remaining > dec!(0) {
        None
    } else {
        Some(cost)
    }
}
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Balance {
    pub asset: String,
    pub balance: rust_decimal::Decimal,
}
