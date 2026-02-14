use anyhow::{anyhow, Result};
use ethers::prelude::*;
use ethers::types::{Address, U256};
use hmac::{Hmac, Mac};
use log::{info, warn};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

// ==================================================
// CONSTANTS (Polygon / Polymarket)
// ==================================================

const POLYMARKET_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const CTF_CONTRACT: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
const USDC_ADDRESS: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const MIN_ALLOWANCE: u128 = 1_000_000; // $1 (6 decimals)
const CLOB_API_URL: &str = "https://clob.polymarket.com";

// ==================================================
// CLIENT (WITH API CREDENTIALS)
// ==================================================

#[derive(Clone)]
pub struct ClobClient {
    pub http: Client,
    provider: Arc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    proxy_wallet: Address,
    read_only: bool,
    // NEW: API credentials for authentication
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    eoa_address: String,
}

impl ClobClient {
    pub async fn new(
        rpc_url: &str,
        private_key: &str,
        proxy_wallet: &str,
        api_key: String,
        api_secret: String,
        api_passphrase: String,
    ) -> Result<Self> {
        let wallet: LocalWallet = private_key.parse()?;
        let provider = Provider::<Http>::try_from(rpc_url)?;
        let chain_id = provider.get_chainid().await?.as_u64();
        let wallet = wallet.with_chain_id(chain_id);
        use ethers::utils::to_checksum;
        let eoa_address = to_checksum(&wallet.address(), None);

        let signer = Arc::new(SignerMiddleware::new(provider, wallet));

        // Check for read-only mode from env
        let read_only = std::env::var("READ_ONLY")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .unwrap_or(false);

        if read_only {
            warn!("⚠️  READ-ONLY MODE ENABLED - No real orders will be submitted");
        }

        Ok(Self {
            http: Client::new(),
            provider: signer,
            proxy_wallet: Address::from_str(proxy_wallet)?,
            read_only,
            api_key,
            api_secret,
            api_passphrase,
            eoa_address,
        })
    }

    // ==================================================
    // TRADING READINESS CHECK
    // ==================================================

    pub async fn ensure_trading_ready(&self, required_usdc: u128) -> Result<()> {
        self.ensure_balance(required_usdc).await?;

        if self.proxy_is_contract().await? {
            self.ensure_safe_checks().await?;
        } else {
            self.ensure_usdc_allowance().await?;
            self.ensure_erc1155_approval().await?;
        }

        Ok(())
    }

    async fn proxy_is_contract(&self) -> Result<bool> {
        let code = self
            .provider
            .provider()
            .get_code(self.proxy_wallet, None)
            .await?;
        Ok(!code.0.is_empty())
    }

    async fn ensure_balance(&self, required: u128) -> Result<()> {
        let bal = self.usdc().balance_of(self.proxy_wallet).call().await?;
        if bal < U256::from(required) {
            return Err(anyhow!(
                "❌ Insufficient USDC balance. Need: {}, Have: {}",
                required as f64 / 1_000_000.0,
                bal.as_u128() as f64 / 1_000_000.0
            ));
        }
        info!(
            "✅ USDC balance OK: ${:.2}",
            bal.as_u128() as f64 / 1_000_000.0
        );
        Ok(())
    }

    async fn ensure_safe_checks(&self) -> Result<()> {
        let allowance = self
            .usdc()
            .allowance(self.proxy_wallet, self.exchange())
            .call()
            .await?;

        if allowance < U256::from(MIN_ALLOWANCE) {
            return Err(anyhow!(
                "❌ USDC allowance missing on Gnosis Safe. Please approve in Polymarket UI."
            ));
        }

        let approved = self
            .ctf()
            .is_approved_for_all(self.proxy_wallet, self.exchange())
            .call()
            .await?;

        if !approved {
            return Err(anyhow!(
                "❌ ERC-1155 approval missing on Gnosis Safe. Please approve in Polymarket UI."
            ));
        }

        info!("✅ Gnosis Safe approvals OK");
        Ok(())
    }

    async fn ensure_usdc_allowance(&self) -> Result<()> {
        let allowance = self
            .usdc()
            .allowance(self.proxy_wallet, self.exchange())
            .call()
            .await?;

        if allowance >= U256::from(MIN_ALLOWANCE) {
            info!("✅ USDC allowance OK");
            return Ok(());
        }

        warn!("⚠️  Approving USDC spending to Polymarket exchange...");
        let tx = self
            .usdc()
            .approve(self.exchange(), U256::MAX)
            .send()
            .await?
            .await?;

        info!("✅ USDC approved. Tx: {:?}", tx);
        Ok(())
    }

    async fn ensure_erc1155_approval(&self) -> Result<()> {
        let approved = self
            .ctf()
            .is_approved_for_all(self.proxy_wallet, self.exchange())
            .call()
            .await?;

        if approved {
            info!("✅ ERC-1155 approval OK");
            return Ok(());
        }

        warn!("⚠️  Approving ERC-1155 (CTF) to Polymarket exchange...");
        let tx = self
            .ctf()
            .set_approval_for_all(self.exchange(), true)
            .send()
            .await?
            .await?;

        info!("✅ ERC-1155 approved. Tx: {:?}", tx);
        Ok(())
    }

    // ==================================================
    // HMAC SIGNATURE GENERATION
    // ==================================================

    fn generate_hmac_signature(
        &self,
        timestamp: u64,
        method: &str,
        path: &str,
        body: &str,
    ) -> String {
        let message = format!("{}{}{}{}", timestamp, method, path, body);
        eprintln!("DEBUG HMAC - Timestamp: {}", timestamp);
        eprintln!("DEBUG HMAC - Method: {}", method);
        eprintln!("DEBUG HMAC - Path: {}", path);
        eprintln!("DEBUG HMAC - Body length: {}", body.len());
        eprintln!("DEBUG HMAC - Message: {}", message);
        use base64::{engine::general_purpose, Engine as _};
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let secret_bytes = general_purpose::URL_SAFE
            .decode(&self.api_secret)
            .expect("API secret must be valid base64");

        let mut mac =
            Hmac::<Sha256>::new_from_slice(&secret_bytes).expect("HMAC can take key of any size");
        mac.update(message.as_bytes());

        let result = mac.finalize();
        let code_bytes = result.into_bytes();

        general_purpose::URL_SAFE.encode(&code_bytes)
    }

    // ==================================================
    // ORDER SUBMISSION - WITH AUTHENTICATION
    // ==================================================

    pub async fn submit_order(
        &self,
        order: crate::wallet::signer::ClobOrder,
        sig: Signature,
        proxy: &str,
    ) -> Result<()> {
        if self.read_only {
            info!("📝 [READ-ONLY] Would submit order:");
            info!("   Token: 0x{}", hex::encode(order.token_id.as_bytes()));
            info!("   Side: {}", if order.side == 0 { "BUY" } else { "SELL" });
            info!(
                "   Maker Amount: {:.6}",
                order.maker_amount.as_u128() as f64 / 1_000_000.0
            );
            info!(
                "   Taker Amount: {:.6}",
                order.taker_amount.as_u128() as f64 / 1_000_000.0
            );
            return Ok(());
        }

        // Polymarket CLOB API order format
        #[derive(Serialize, Debug)]
        struct ClobOrderPayload {
            salt: String,
            maker: String,
            signer: String,
            taker: String,

            #[serde(rename = "tokenId")]
            token_id: String,

            #[serde(rename = "makerAmount")]
            maker_amount: String,

            #[serde(rename = "takerAmount")]
            taker_amount: String,

            side: String,

            #[serde(rename = "feeRateBps")]
            fee_rate_bps: String,

            nonce: String,
            expiration: String,
            signature: String,

            #[serde(rename = "signatureType")]
            signature_type: u8,
        }

        // Generate random salt
        use ::rand::Rng; // Use external rand crate explicitly
        let salt = ::rand::random::<u64>().to_string();

        // Use the amounts from the order (already calculated)
        let maker_amount = format!("{}", order.maker_amount.as_u128());
        let taker_amount = format!("{}", order.taker_amount.as_u128());

        let payload = ClobOrderPayload {
            salt,
            maker: proxy.to_string(),
            signer: self.eoa_address.clone(),
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id: format!("{:#x}", order.token_id),
            maker_amount,
            taker_amount,
            side: if order.side == 0 { "BUY" } else { "SELL" }.to_string(),
            fee_rate_bps: "0".to_string(),
            nonce: order.nonce.to_string(),
            expiration: order.expiration.to_string(),
            signature: {
                // Manually construct signature bytes: r (32) + s (32) + v (1) = 65 bytes
                let mut sig_bytes = [0u8; 65];
                sig.r.to_big_endian(&mut sig_bytes[0..32]);
                sig.s.to_big_endian(&mut sig_bytes[32..64]);
                sig_bytes[64] = sig.v as u8;
                format!("0x{}", hex::encode(&sig_bytes))
            },
            signature_type: 0, // 0 = EOA (MetaMask)
        };

        info!("📤 Submitting order to CLOB API...");
        info!("   Token: {}", &payload.token_id[..16]);
        info!(
            "   {} maker={} taker={}",
            payload.side, payload.maker_amount, payload.taker_amount
        );

        // Generate authentication headers
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        let path = "/order";
        let body = serde_json::to_string(&payload)?;
        let signature = self.generate_hmac_signature(timestamp, "POST", path, &body);

        let url = format!("{}{}", CLOB_API_URL, path);

        // Make authenticated request
        let resp = self
            .http
            .post(&url)
            .header("POLY-ADDRESS", &self.eoa_address)
            .header("POLY-API-KEY", &self.api_key)
            .header("POLY-SIGNATURE", &signature)
            .header("POLY-TIMESTAMP", timestamp.to_string())
            .header("POLY-PASSPHRASE", &self.api_passphrase)
            .header("Content-Type", "application/json")
            .body(body)
            .timeout(std::time::Duration::from_secs(10));

        // Debug: print headers being sent
        eprintln!("=== RUST HEADERS ===");
        eprintln!("POLY-ADDRESS: {}", self.eoa_address);
        eprintln!("POLY-API-KEY: {}", self.api_key);
        eprintln!("POLY-SIGNATURE: {}", signature);
        eprintln!("POLY-TIMESTAMP: {}", timestamp);
        eprintln!("POLY-PASSPHRASE: {}", self.api_passphrase);

        let resp = resp.send().await?;

        let status = resp.status();
        let body = resp.text().await?;

        if !status.is_success() {
            warn!("❌ Order rejected by CLOB API");
            warn!("   Status: {}", status);
            warn!("   Response: {}", body);
            return Err(anyhow!("Order rejected: {} - {}", status, body));
        }

        // Parse response to get order ID
        #[derive(Deserialize)]
        struct OrderResponse {
            order_id: Option<String>,
            success: Option<bool>,
        }

        match serde_json::from_str::<OrderResponse>(&body) {
            Ok(resp) => {
                if let Some(order_id) = resp.order_id {
                    info!("✅ Order submitted! ID: {}", order_id);
                } else {
                    info!("✅ Order submitted! {}", body);
                }
            }
            Err(_) => {
                info!("✅ Order submitted! {}", body);
            }
        }

        Ok(())
    }

    // ==================================================
    // STUBS FOR FUTURE
    // ==================================================

    pub async fn get_orderbook(&self, _token_id: &str) -> Result<()> {
        Err(anyhow!("Use execution::orderbook::fetch_orderbook instead"))
    }

    pub fn best_price(&self, _book: &(), _side: u8) -> Result<()> {
        Err(anyhow!("Use execution::orderbook methods instead"))
    }

    // ==================================================
    // CONTRACT HELPERS
    // ==================================================

    fn exchange(&self) -> Address {
        Address::from_str(POLYMARKET_EXCHANGE).unwrap()
    }

    fn usdc(&self) -> USDCContract<SignerMiddleware<Provider<Http>, LocalWallet>> {
        USDCContract::new(
            Address::from_str(USDC_ADDRESS).unwrap(),
            self.provider.clone(),
        )
    }

    fn ctf(&self) -> CTFContract<SignerMiddleware<Provider<Http>, LocalWallet>> {
        CTFContract::new(
            Address::from_str(CTF_CONTRACT).unwrap(),
            self.provider.clone(),
        )
    }
}

// ==================================================
// ABI GENERATION
// ==================================================

abigen!(
    USDCContract,
    r#"[
        function balanceOf(address) view returns (uint256)
        function allowance(address,address) view returns (uint256)
        function approve(address,uint256) returns (bool)
    ]"#
);

abigen!(
    CTFContract,
    r#"[
        function isApprovedForAll(address,address) view returns (bool)
        function setApprovalForAll(address,bool)
    ]"#
);
