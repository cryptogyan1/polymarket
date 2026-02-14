use clap::Parser;
use serde::{Deserialize, Serialize};
use std::env;
use std::path::PathBuf;

/* =======================
POSITION SIZING MODES
======================= */

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TradeMode {
    Fixed,
    Percentage,
    Dynamic,
    Free,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSizing {
    pub mode: TradeMode,

    // FIXED: absolute USDC per trade
    pub fixed_usdc: Option<f64>,

    // PERCENTAGE: % of wallet balance
    pub percentage: Option<f64>,

    // DYNAMIC: max risk % of balance
    pub max_risk_percent: Option<f64>,
}

impl PositionSizing {
    pub fn from_env() -> Self {
        let mode = env::var("TRADE_MODE")
            .unwrap_or_else(|_| "PERCENTAGE".to_string())
            .to_uppercase();

        match mode.as_str() {
            "FIXED" => Self {
                mode: TradeMode::Fixed,
                fixed_usdc: Some(
                    env::var("FIXED_USDC_PER_TRADE")
                        .unwrap_or_else(|_| "2".to_string())
                        .parse()
                        .expect("Invalid FIXED_USDC_PER_TRADE"),
                ),
                percentage: None,
                max_risk_percent: None,
            },

            "DYNAMIC" => Self {
                mode: TradeMode::Dynamic,
                fixed_usdc: None,
                percentage: None,
                max_risk_percent: Some(
                    env::var("MAX_RISK_PERCENT")
                        .unwrap_or_else(|_| "1".to_string())
                        .parse()
                        .expect("Invalid MAX_RISK_PERCENT"),
                ),
            },

            "FREE" => Self {
                mode: TradeMode::Free,
                fixed_usdc: None,
                percentage: None,
                max_risk_percent: None,
            },

            // DEFAULT = PERCENTAGE
            _ => Self {
                mode: TradeMode::Percentage,
                fixed_usdc: None,
                percentage: Some(
                    env::var("PERCENTAGE_PER_TRADE")
                        .unwrap_or_else(|_| "10".to_string())
                        .parse()
                        .expect("Invalid PERCENTAGE_PER_TRADE"),
                ),
                max_risk_percent: None,
            },
        }
    }
}

/* =======================
WALLET CONFIG
======================= */

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletConfig {
    pub private_key: Option<String>,
    pub chain_id: u64,
    pub proxy_wallet: String,
}

/* =======================
CLI ARGS
======================= */

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args {
    /// Configuration file path
    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,
}

/* =======================
MAIN CONFIG
======================= */

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub polymarket: PolymarketConfig,
    pub trading: TradingConfig,
    pub wallet: WalletConfig,
}

/* =======================
POLYMARKET CONFIG
======================= */

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolymarketConfig {
    pub gamma_api_url: String,
    pub clob_api_url: String,
    pub ws_url: String,

    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub api_passphrase: Option<String>,
}

/* =======================
TRADING CONFIG
======================= */

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub min_profit_threshold: f64,

    // 🧠 POSITION SIZING (THIS WAS MISSING / BROKEN BEFORE)
    pub position_sizing: PositionSizing,

    pub eth_condition_id: Option<String>,
    pub btc_condition_id: Option<String>,

    pub check_interval_ms: u64,
}

/* =======================
DEFAULT CONFIG
======================= */

impl Default for Config {
    fn default() -> Self {
        Self {
            polymarket: PolymarketConfig {
                gamma_api_url: "https://gamma-api.polymarket.com".to_string(),
                clob_api_url: "https://clob.polymarket.com".to_string(),
                ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
                api_key: None,
                api_secret: None,
                api_passphrase: None,
            },
            trading: TradingConfig {
                min_profit_threshold: 0.005,
                position_sizing: PositionSizing {
                    mode: TradeMode::Percentage,
                    fixed_usdc: None,
                    percentage: Some(10.0),
                    max_risk_percent: None,
                },
                eth_condition_id: None,
                btc_condition_id: None,
                check_interval_ms: 250,
            },
            wallet: WalletConfig {
                private_key: None,
                chain_id: 137,
                proxy_wallet: String::new(),
            },
        }
    }
}

/* =======================
LOAD / CREATE CONFIG
======================= */

impl Config {
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content)?)
        } else {
            let cfg = Config::default();
            let content = serde_json::to_string_pretty(&cfg)?;
            std::fs::write(path, content)?;
            Ok(cfg)
        }
    }
}
// ==================================================
// ENVIRONMENT HELPERS
// ==================================================

impl Config {
    /// Check if running in read-only mode
    pub fn is_read_only() -> bool {
        std::env::var("READ_ONLY")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .unwrap_or(false)
    }

    /// Get minimum trade size in USDC
    pub fn min_trade_size() -> f64 {
        std::env::var("MIN_TRADE_SIZE")
            .unwrap_or_else(|_| "1.0".to_string())
            .parse()
            .unwrap_or(1.0)
    }

    /// Get maximum trade size in USDC
    pub fn max_trade_size() -> f64 {
        std::env::var("MAX_TRADE_SIZE")
            .unwrap_or_else(|_| "100.0".to_string())
            .parse()
            .unwrap_or(100.0)
    }
}
