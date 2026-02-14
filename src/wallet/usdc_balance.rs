use anyhow::Result;
use ethers::prelude::*;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
abigen!(
    Usdc,
    r#"[
        function balanceOf(address) view returns (uint256)
        function decimals() view returns (uint8)
    ]"#
);
pub async fn get_usdc_balance(
    rpc_url: &str,
    usdc_address: &str,
    wallet: &str,
) -> Result<Decimal> {
    let provider = Provider::<Http>::try_from(rpc_url)?;
    let provider = Arc::new(provider);  // ← Wrap in Arc
    let usdc = Usdc::new(usdc_address.parse()?, provider);
    let raw = usdc.balance_of(wallet.parse()?).call().await?;
    let decimals = usdc.decimals().call().await?;
    let scale = 10u128.pow(decimals as u32);
    Ok(Decimal::from(raw.as_u128()) / Decimal::from(scale))
}
