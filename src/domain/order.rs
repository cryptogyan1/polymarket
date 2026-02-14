use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClobOrder {
    pub maker: String,  // proxy wallet
    pub signer: String, // EOA
    pub token_id: String,
    pub side: String,   // "buy" | "sell"
    pub price: String,  // decimal string
    pub amount: String, // decimal string (USDC)
    pub expiration: u64,
    pub nonce: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedOrder {
    pub order: ClobOrder,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PricedOrder {
    pub token_id: String,
    pub side: Side,
    pub price: f64,
    pub size_usdc: f64,
}
