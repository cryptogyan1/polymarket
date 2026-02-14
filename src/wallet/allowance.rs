use anyhow::{anyhow, Result};
use ethers::abi::Abi;
use ethers::contract::Contract;
use ethers::providers::Middleware;
use ethers::types::{Address, U256};
use serde_json::from_slice;
use std::str::FromStr;
use std::sync::Arc;

// ===============================
// CONSTANTS
// ===============================

const USDC_ADDRESS: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

const CTF_ADDRESS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";

const POLYMARKET_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";

const MIN_ALLOWANCE: u128 = 1_000_000; // 1 USDC (6 decimals)

// ===============================
// ABI LOADERS (THE FIX 🔥)
// ===============================

fn load_erc20_abi() -> Result<Abi> {
    Ok(from_slice(include_bytes!("../abi/erc20.json"))?)
}

fn load_erc1155_abi() -> Result<Abi> {
    Ok(from_slice(include_bytes!("../abi/erc1155.json"))?)
}

// ===============================
// MAIN ENTRY — STAGE 2 GATEKEEPER
// ===============================

pub async fn verify_allowances<M: Middleware + 'static>(
    provider: Arc<M>,
    proxy_wallet: &str,
) -> Result<()> {
    let proxy: Address = proxy_wallet.parse()?;
    let exchange: Address = POLYMARKET_EXCHANGE.parse()?;

    let usdc_abi = load_erc20_abi()?;
    let erc1155_abi = load_erc1155_abi()?;

    let usdc = Contract::new(Address::from_str(USDC_ADDRESS)?, usdc_abi, provider.clone());

    let ctf = Contract::new(
        Address::from_str(CTF_ADDRESS)?,
        erc1155_abi,
        provider.clone(),
    );

    // ===============================
    // USDC ALLOWANCE
    // ===============================
    let allowance: U256 = usdc.method("allowance", (proxy, exchange))?.call().await?;

    if allowance < U256::from(MIN_ALLOWANCE) {
        return Err(anyhow!(
            "❌ USDC allowance missing — approve Polymarket exchange in UI"
        ));
    }

    // ===============================
    // ERC-1155 APPROVAL
    // ===============================
    let approved: bool = ctf
        .method("isApprovedForAll", (proxy, exchange))?
        .call()
        .await?;

    if !approved {
        return Err(anyhow!(
            "❌ ERC-1155 approval missing — enable trading in Polymarket UI"
        ));
    }

    Ok(())
}
