use crate::domain::order::Side;
use crate::domain::*;
use crate::execution::clob_client::ClobClient;
use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Client;
use rust_decimal::Decimal;
use sha2::Sha256;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct PolymarketClient {
    client: Client,

    pub gamma_url: String,
    pub clob_url: String,

    pub api_key: String,
    api_secret: String,
    api_passphrase: String,

    pub read_only: bool,

    pub clob_client: Option<Arc<ClobClient>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SignedOrderPayload {
    pub order: OrderRequest,
    pub signature: String,
    pub address: String,
}

// ==================================================
// CONSTRUCTOR
// ==================================================
impl PolymarketClient {
    pub fn new(
        gamma_url: String,
        clob_url: String,
        api_key: String,
        api_secret: String,
        api_passphrase: String,
        read_only: bool,
        clob_client: Option<Arc<ClobClient>>,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("HTTP client");

        Self {
            client,
            clob_client,
            gamma_url,
            clob_url,
            api_key,
            api_secret,
            api_passphrase,
            read_only,
        }
    }

    // ==================================================
    // EXPOSE CLOB CLIENT
    // ==================================================
    pub fn clob_client(&self) -> Option<Arc<ClobClient>> {
        self.clob_client.clone()
    }

    pub fn http_client(&self) -> &Client {
        &self.client
    }

    // ==================================================
    // BUILD + SIGN ORDER (🔥 THIS WAS MISSING)
    // ==================================================
    pub fn build_signed_order(
        &self,
        priced: &crate::domain::order::PricedOrder,
    ) -> Result<SignedOrderPayload> {
        let order = OrderRequest {
            token_id: priced.token_id.clone(),
            side: match priced.side {
                Side::Buy => "buy".to_string(),
                Side::Sell => "sell".to_string(),
            },
            price: priced.price.to_string(),
            size: priced.size_usdc.to_string(),
            order_type: match priced.side {
                Side::Buy => "buy".to_string(),
                Side::Sell => "sell".to_string(),
            },
        };

        let body = serde_json::to_string(&order)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs()
            .to_string();

        let signature = self.sign_request("POST", "/orders", &body, &timestamp);

        Ok(SignedOrderPayload {
            order,
            signature,
            address: self.api_key.clone(),
        })
    }

    // ==================================================
    // REQUEST SIGNING (HMAC)
    // ==================================================
    fn sign_request(&self, method: &str, path: &str, body: &str, timestamp: &str) -> String {
        let payload = format!("{}{}{}{}", timestamp, method, path, body);

        let mut mac =
            HmacSha256::new_from_slice(self.api_secret.as_bytes()).expect("HMAC init failed");

        mac.update(payload.as_bytes());
        general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    // ==================================================
    // PLACE ORDER
    // ==================================================
    pub async fn place_signed_order(&self, order: &OrderRequest) -> Result<OrderResponse> {
        if self.read_only {
            anyhow::bail!("READ ONLY MODE");
        }

        let payload = SignedOrderPayload {
            order: order.clone(),
            signature: "".to_string(), // Polymarket REST signs via headers
            address: "".to_string(),
        };

        let path = "/orders";
        let url = format!("{}{}", self.clob_url, path);
        let body = serde_json::to_string(&payload)?;

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs()
            .to_string();

        let signature = self.sign_request("POST", path, &body, &timestamp);

        let response = self
            .client
            .post(&url)
            .header("POLY-API-KEY", &self.api_key)
            .header("POLY-API-SIGNATURE", signature)
            .header("POLY-API-TIMESTAMP", &timestamp)
            .header("POLY-API-PASSPHRASE", &self.api_passphrase)
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let err = response.text().await.unwrap_or_default();
            anyhow::bail!("Order rejected: {}", err);
        }

        Ok(response.json().await?)
    }

    // ==================================================
    // 🔥 FIXED: GET USDC BALANCE WITH PROPER AUTH
    // ==================================================
    pub async fn get_usdc_balance(&self) -> Result<Decimal> {
        // Get balance directly from blockchain (more reliable)
        let proxy_wallet = std::env::var("PROXY_WALLET").context("PROXY_WALLET not set")?;

        use ethers::prelude::*;
        use std::str::FromStr;

        const USDC_ADDRESS: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

        abigen!(
            USDC,
            r#"[function balanceOf(address) view returns (uint256)]"#
        );

        let rpc_url = std::env::var("RPC_URL").context("RPC_URL not set")?;

        let provider = Provider::<Http>::try_from(&rpc_url)?;
        let usdc = USDC::new(Address::from_str(USDC_ADDRESS)?, Arc::new(provider));

        let wallet_addr = Address::from_str(&proxy_wallet)?;
        let balance = usdc.balance_of(wallet_addr).call().await?;

        // Convert from 6 decimals to Decimal
        let balance_f64 = balance.as_u128() as f64 / 1_000_000.0;
        Ok(Decimal::try_from(balance_f64).unwrap_or_default())
    }

    pub async fn list_markets(&self, limit: usize, offset: usize) -> Result<Vec<Market>> {
        let url = format!(
            "{}/markets?active=true&closed=false&limit={}&offset={}",
            self.gamma_url, limit, offset
        );

        let response = self.client.get(&url).send().await?.error_for_status()?;
        let markets: Vec<Market> = response.json().await?;
        Ok(markets)
    }

    // ==================================================
    // GET MARKET BY SLUG (Gamma)
    // ==================================================
    pub async fn get_market_by_slug(&self, slug: &str) -> Result<Market> {
        let url = format!("{}/events/slug/{}", self.gamma_url, slug);

        let response = self.client.get(&url).send().await?.error_for_status()?; // 👈 better error handling

        let json: serde_json::Value = response.json().await?;

        json["markets"]
            .as_array()
            .and_then(|m| m.first())
            .map(|m| serde_json::from_value(m.clone()).unwrap())
            .context("Market not found")
    }
}
