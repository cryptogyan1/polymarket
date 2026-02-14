use anyhow::{anyhow, Result};
use ethers::prelude::*;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::sync::Arc;

// ================================
// USDC ABI (balanceOf only)
// ================================
abigen!(
    UsdcContract,
    r#"[
        function balanceOf(address owner) view returns (uint256)
        function decimals() view returns (uint8)
    ]"#
);

// ================================
// SIMPLE ONE-SHOT BALANCE FETCH
// ================================
pub async fn get_usdc_balance(rpc_url: &str, usdc_contract: &str, wallet: &str) -> Result<Decimal> {
    let provider = Provider::<Http>::try_from(rpc_url).map_err(|e| anyhow!("RPC error: {}", e))?;

    let provider = Arc::new(provider);

    let contract_addr: Address = usdc_contract.parse()?;
    let wallet_addr: Address = wallet.parse()?;

    let usdc = UsdcContract::new(contract_addr, provider);

    let raw_balance = usdc.balance_of(wallet_addr).call().await?;
    let decimals = usdc.decimals().call().await?;

    let divisor = Decimal::from_u128(10u128.pow(decimals as u32))
        .ok_or_else(|| anyhow!("Decimal conversion failed"))?;

    Ok(Decimal::from_u128(raw_balance.as_u128()).unwrap() / divisor)
}

// ================================
// OPTIONAL: STATEFUL TRACKER
// ================================
#[derive(Clone)]
pub struct BalanceTracker {
    pub last_balance: Decimal,
}

impl BalanceTracker {
    pub fn new() -> Self {
        Self {
            last_balance: Decimal::ZERO,
        }
    }
}
