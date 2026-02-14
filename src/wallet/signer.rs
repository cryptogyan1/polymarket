use anyhow::Result;
use ethers::contract::EthAbiType;
use ethers::prelude::*;
use ethers::types::{H256, U256};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct WalletSigner {
    wallet: LocalWallet,
}

impl WalletSigner {
    pub fn new(private_key: &str, chain_id: u64) -> Result<Self> {
        let wallet: LocalWallet = private_key.parse()?;
        Ok(Self {
            wallet: wallet.with_chain_id(chain_id),
        })
    }

    pub fn address(&self) -> Address {
        self.wallet.address()
    }

    pub async fn sign_order(&self, order: &ClobOrder) -> Result<Signature> {
        Ok(self.wallet.sign_typed_data(order).await?)
    }
}

/// =================================================
/// Polymarket CLOB Order — EIP-712 (CORRECT)
/// =================================================
#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    EthAbiType,
    Eip712, // ✅ THIS WAS MISSING
)]
#[eip712(
    name = "Polymarket CTF Exchange",
    version = "1",
    chain_id = 137,
    verifying_contract = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"
)]
pub struct ClobOrder {
    pub salt: U256,
    pub maker: Address,
    pub signer: Address,
    pub taker: Address,
    #[serde(rename = "tokenId")]
    pub token_id: H256,
    #[serde(rename = "makerAmount")]
    pub maker_amount: U256,
    #[serde(rename = "takerAmount")]
    pub taker_amount: U256,
    pub side: u8,
    #[serde(rename = "feeRateBps")]
    pub fee_rate_bps: U256,
    pub nonce: U256,
    pub expiration: U256,
}
