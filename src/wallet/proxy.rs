use anyhow::Result;
use ethers::prelude::*;
use ethers::types::Address;
use std::str::FromStr;
use std::sync::Arc;

pub async fn is_gnosis_safe(provider: Arc<Provider<Http>>, proxy_wallet: &str) -> Result<bool> {
    let addr = Address::from_str(proxy_wallet)?;
    let code = provider.get_code(addr, None).await?;
    Ok(!code.0.is_empty())
}
