use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::domain::order::Side;

#[derive(Clone)]
pub struct ExecutorClient {
    base_url: String,
    http: Client,
    fok_enabled: bool,
    allow_partial_arb: bool,
}

#[derive(Debug, Serialize)]
pub struct ExecuteOrderRequest {
    pub token_id: String,
    pub side: String,
    pub price: f64,
    pub size_usdc: f64,
    pub fok: bool,
}

#[derive(Debug, Deserialize)]
pub struct ExecuteOrderResponse {
    pub ok: bool,
    pub order_id: Option<String>,
    pub error: Option<String>,
}

impl ExecutorClient {
    pub fn new(base_url: String) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("failed to build HTTP client for executor")?;

        let fok_enabled = std::env::var("FOK")
            .unwrap_or_else(|_| "false".to_string())
            .trim()
            .eq_ignore_ascii_case("true");

        let allow_partial_arb = std::env::var("ALLOW_PARTIAL_ARB")
            .unwrap_or_else(|_| "false".to_string())
            .trim()
            .eq_ignore_ascii_case("true");

        Ok(Self {
            base_url,
            http,
            fok_enabled,
            allow_partial_arb,
        })
    }

    pub fn fok_enabled(&self) -> bool {
        self.fok_enabled
    }

    pub fn allow_partial_arb(&self) -> bool {
        self.allow_partial_arb
    }

    pub async fn healthcheck(&self) -> Result<()> {
        let url = format!("{}/health", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("executor unreachable")?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "executor healthcheck failed with status {}",
                resp.status()
            ));
        }
        Ok(())
    }

    pub async fn execute_order(
        &self,
        token_id: &str,
        side: Side,
        price: f64,
        size_usdc: f64,
    ) -> Result<ExecuteOrderResponse> {
        let url = format!("{}/execute", self.base_url.trim_end_matches('/'));
        let side = match side {
            Side::Buy => "BUY".to_string(),
            Side::Sell => "SELL".to_string(),
        };

        let payload = ExecuteOrderRequest {
            token_id: token_id.to_string(),
            side,
            price,
            size_usdc,
            fok: self.fok_enabled,
        };

        let resp = self
            .http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("failed to send order intent to executor")?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .context("failed to read executor response body")?;

        if !status.is_success() {
            let detail = parse_error_detail(&body);
            return Err(anyhow!(
                "executor rejected order intent (status {}): {}",
                status,
                detail
            ));
        }

        let parsed: ExecuteOrderResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "executor returned invalid JSON for success response: {}",
                body
            )
        })?;

        if !parsed.ok {
            return Err(anyhow!(
                "executor rejected order intent (status {}): {}",
                status,
                parsed.error.unwrap_or_else(|| "unknown error".to_string())
            ));
        }

        Ok(parsed)
    }
}

fn parse_error_detail(body: &str) -> String {
    let parsed_json: Result<Value, _> = serde_json::from_str(body);
    if let Ok(value) = parsed_json {
        if let Some(detail) = value.get("detail").and_then(Value::as_str) {
            return detail.to_string();
        }

        if let Some(error) = value.get("error").and_then(Value::as_str) {
            return error.to_string();
        }
    }

    body.to_string()
}
