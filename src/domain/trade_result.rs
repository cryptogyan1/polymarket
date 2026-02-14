#[derive(Debug, Clone)]
pub enum TradeResult {
    SkippedInsufficientBalance {
        available: f64,
        required: f64,
    },
    SkippedTooSmall,
    Executed {
        filled_usdc: f64,
    },
    Rejected(String),
}
