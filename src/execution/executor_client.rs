use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
pub struct TelegramNotifyRequest {
    pub r#type: String,
    pub data: Value,
}

#[derive(Debug, Deserialize)]
pub struct TelegramNotifyResponse {
    pub ok: bool,
    pub error: Option<String>,
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

#[derive(Debug, Serialize)]
pub struct CashoutRequest {
    pub token_id: String,
    pub shares: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct CashoutResponse {
    pub ok: bool,
    pub token_id: String,
    pub requested_shares: Option<f64>,
    pub order_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PositionResponse {
    pub ok: bool,
    pub token_id: String,
    pub shares: f64,
}

#[derive(Debug, Serialize)]
pub struct CancelOrderRequest {
    pub order_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CancelOrderResponse {
    pub ok: bool,
    pub order_id: String,
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
        self.execute_order_with_fok(token_id, side, price, size_usdc, self.fok_enabled)
            .await
    }

    pub async fn execute_order_with_fok(
        &self,
        token_id: &str,
        side: Side,
        price: f64,
        size_usdc: f64,
        fok: bool,
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
            fok,
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

    pub async fn get_token_shares(&self, token_id: &str) -> Result<f64> {
        let url = format!(
            "{}/position/{}",
            self.base_url.trim_end_matches('/'),
            token_id
        );

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("failed to fetch token position from executor")?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .context("failed to read executor position response body")?;

        if !status.is_success() {
            let detail = parse_error_detail(&body);
            return Err(anyhow!(
                "executor rejected position query (status {}): {}",
                status,
                detail
            ));
        }

        let parsed: PositionResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "executor returned invalid JSON for position success response: {}",
                body
            )
        })?;

        if !parsed.ok {
            return Err(anyhow!(
                "executor returned unsuccessful position query (status {}): token={}",
                status,
                parsed.token_id
            ));
        }

        Ok(parsed.shares)
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<CancelOrderResponse> {
        let url = format!("{}/cancel", self.base_url.trim_end_matches('/'));

        let payload = CancelOrderRequest {
            order_id: order_id.to_string(),
        };

        let resp = self
            .http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("failed to send cancel intent to executor")?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .context("failed to read executor cancel response body")?;

        if !status.is_success() {
            let detail = parse_error_detail(&body);
            return Err(anyhow!(
                "executor rejected cancel intent (status {}): {}",
                status,
                detail
            ));
        }

        let parsed: CancelOrderResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "executor returned invalid JSON for cancel success response: {}",
                body
            )
        })?;

        if !parsed.ok {
            return Err(anyhow!(
                "executor rejected cancel intent (status {}): {}",
                status,
                parsed.error.unwrap_or_else(|| "unknown error".to_string())
            ));
        }

        Ok(parsed)
    }

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("failed to fetch token position from executor")?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .context("failed to read executor position response body")?;

        if !status.is_success() {
            let detail = parse_error_detail(&body);
            return Err(anyhow!(
                "executor rejected position query (status {}): {}",
                status,
                detail
            ));
        }

        let parsed: PositionResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "executor returned invalid JSON for position success response: {}",
                body
            )
        })?;

        if !parsed.ok {
            return Err(anyhow!(
                "executor returned unsuccessful position query (status {}): token={}",
                status,
                parsed.token_id
            ));
        }

        Ok(parsed.shares)
    }
    pub async fn notify_trade_success(
        &self,
        direction: &str,
        leg1_token: &str,
        leg1_price: f64,
        leg1_order_id: Option<&str>,
        leg2_token: &str,
        leg2_price: f64,
        leg2_order_id: Option<&str>,
        total_cost: f64,
        combined_price: f64,
        target_price: f64,
    ) -> Result<()> {
        let payload = TelegramNotifyRequest {
            r#type: "success".to_string(),
            data: json!({
                "direction": direction,
                "leg1": {
                    "token": leg1_token,
                    "price": leg1_price,
                    "filled": true,
                    "order_id": leg1_order_id,
                },
                "leg2": {
                    "token": leg2_token,
                    "price": leg2_price,
                    "filled": true,
                    "order_id": leg2_order_id,
                },
                "total_cost": total_cost,
                "combined_price": combined_price,
                "target_price": target_price,
            }),
        };

        self.notify(&payload).await
    }

    async fn notify(&self, payload: &TelegramNotifyRequest) -> Result<()> {
        let url = format!("{}/notify", self.base_url.trim_end_matches('/'));

        let resp = self
            .http
            .post(&url)
            .json(payload)
            .send()
            .await
            .context("failed to send telegram notification to executor")?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .context("failed to read executor notify response body")?;

        if !status.is_success() {
            let detail = parse_error_detail(&body);
            return Err(anyhow!(
                "executor notify endpoint failed (status {}): {}",
                status,
                detail
            ));
        }

        let parsed: TelegramNotifyResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "executor returned invalid JSON for notify success response: {}",
                body
            )
        })?;

        if !parsed.ok {
            return Err(anyhow!(
                "executor notify endpoint rejected payload: {}",
                parsed.error.unwrap_or_else(|| "unknown error".to_string())
            ));
        }

        Ok(())
    }

    pub async fn cashout_position(&self, token_id: &str, shares: f64) -> Result<CashoutResponse> {
        let url = format!("{}/cashout", self.base_url.trim_end_matches('/'));

        let payload = CashoutRequest {
            token_id: token_id.to_string(),
            shares: Some(shares),
        };

        let resp = self
            .http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("failed to send cashout intent to executor")?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .context("failed to read executor cashout response body")?;

        if !status.is_success() {
            let detail = parse_error_detail(&body);
            return Err(anyhow!(
                "executor rejected cashout intent (status {}): {}",
                status,
                detail
            ));
        }

        let parsed: CashoutResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "executor returned invalid JSON for cashout success response: {}",
                body
            )
        })?;

        if !parsed.ok {
            return Err(anyhow!(
                "executor rejected cashout intent (status {}): {}",
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
