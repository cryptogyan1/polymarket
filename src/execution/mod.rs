pub mod clob_client;
pub mod errors;
pub mod executor_client;
pub mod orderbook;
pub mod trader;

use crate::client::PolymarketClient;
use crate::config::{Config, PositionSizing, TradeMode, TradingConfig, WalletConfig};
use crate::domain::order::Side;
use crate::domain::*;
use crate::wallet::signer::{ClobOrder, WalletSigner};
use anyhow::{anyhow, Result};
use ethers::types::Address;
use ethers::types::{H256, U256};
use ethers::utils::keccak256;
use log::{info, warn};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

pub use clob_client::ClobClient;
pub use executor_client::ExecutorClient;

#[derive(Debug, Clone)]
struct ExecutionEnv {
    min_shares: f64,
    max_shares_cap: Option<f64>,
    second_leg_price_bump_cents: f64,
    strict_share_bounds: bool,
    executor_retry_attempts: usize,
    auto_bump_min_shares: bool,
    per_direction_trade_limit: usize,
    arbitrage_max_sum: f64,
    arbitrage_sum_tolerance: f64,
    trade_fee_bps: f64,
    slippage_bps: f64,
    max_total_shares_per_market: Option<f64>,
}

fn load_execution_env() -> ExecutionEnv {
    ExecutionEnv {
        min_shares: std::env::var("MIN_SHARES")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(5.0),
        max_shares_cap: std::env::var("MAX_SHARES")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0),
        second_leg_price_bump_cents: std::env::var("SECOND_LEG_PRICE_BUMP_CENTS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(2.0)
            .max(0.0),
        strict_share_bounds: std::env::var("STRICT_SHARE_BOUNDS")
            .unwrap_or_else(|_| "true".to_string())
            .trim()
            .eq_ignore_ascii_case("true"),
        executor_retry_attempts: std::env::var("EXECUTOR_RETRY_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(2)
            .max(1),
        auto_bump_min_shares: std::env::var("AUTO_BUMP_MIN_SHARES")
            .unwrap_or_else(|_| "true".to_string())
            .trim()
            .eq_ignore_ascii_case("true"),
        per_direction_trade_limit: std::env::var("MAX_TRADES_PER_DIRECTION_PER_WINDOW")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0),
        arbitrage_max_sum: std::env::var("ARBITRAGE_MAX_SUM")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.978),
        arbitrage_sum_tolerance: std::env::var("ARBITRAGE_SUM_TOLERANCE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.02)
            .max(0.0),
        trade_fee_bps: std::env::var("TRADE_FEE_BPS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(100.0)
            .max(0.0),
        slippage_bps: std::env::var("SLIPPAGE_BPS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(35.0)
            .max(0.0),
        max_total_shares_per_market: std::env::var("MAX_TOTAL_SHARES_PER_MARKET")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0),
    }
}

fn env_settings() -> &'static ExecutionEnv {
    static SETTINGS: std::sync::OnceLock<ExecutionEnv> = std::sync::OnceLock::new();
    SETTINGS.get_or_init(load_execution_env)
}

fn min_shares_from_env() -> f64 {
    env_settings().min_shares
}

fn max_shares_cap_from_env() -> Option<f64> {
    env_settings().max_shares_cap
}

fn second_leg_price_bump_cents_from_env() -> f64 {
    env_settings().second_leg_price_bump_cents
}

fn strict_share_bounds_from_env() -> bool {
    env_settings().strict_share_bounds
}

fn executor_retry_attempts_from_env() -> usize {
    env_settings().executor_retry_attempts
}

fn auto_bump_min_shares_from_env() -> bool {
    env_settings().auto_bump_min_shares
}

fn per_direction_trade_limit_from_env() -> usize {
    env_settings().per_direction_trade_limit
}

fn arbitrage_max_sum_from_env() -> f64 {
    env_settings().arbitrage_max_sum
}

fn arbitrage_sum_tolerance_from_env() -> f64 {
    env_settings().arbitrage_sum_tolerance
}

fn trade_fee_bps_from_env() -> f64 {
    env_settings().trade_fee_bps
}

fn slippage_bps_from_env() -> f64 {
    env_settings().slippage_bps
}

fn max_total_shares_per_market_from_env() -> Option<f64> {
    env_settings().max_total_shares_per_market
}

fn str_to_h256(s: &str) -> H256 {
    H256::from_slice(&keccak256(s.as_bytes()))
}

fn to_u256_scaled(v: Decimal) -> U256 {
    let f = v.to_f64().unwrap_or(0.0);
    U256::from((f * 1_000_000.0) as u128)
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn make_nonce() -> U256 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    U256::from(t)
}

fn effective_buy_price(price: f64) -> f64 {
    let fee_mult = 1.0 + (trade_fee_bps_from_env() / 10_000.0);
    let slip_mult = 1.0 + (slippage_bps_from_env() / 10_000.0);
    price * fee_mult * slip_mult
}

fn fmt_count(limit: usize, count: usize) -> String {
    if limit == 0 {
        format!("{} / unlimited", count)
    } else {
        format!("{} / {}", count, limit)
    }
}

#[derive(Debug, Default)]
struct ExecutionState {
    active_window_key: Option<String>,
    trade_count_by_direction: HashMap<String, usize>,
    shares_by_condition: HashMap<String, f64>,
    total_effective_cost_by_condition: HashMap<String, f64>,
}

impl ExecutionState {
    fn reset_for_window(&mut self, window_key: String) {
        if self.active_window_key.as_ref() != Some(&window_key) {
            self.active_window_key = Some(window_key);
            self.trade_count_by_direction.clear();
            self.shares_by_condition.clear();
            self.total_effective_cost_by_condition.clear();
        }
    }

    fn direction_count(&self, direction: &str) -> usize {
        *self.trade_count_by_direction.get(direction).unwrap_or(&0)
    }

    fn increment_direction(&mut self, direction: &str) {
        let next = self.direction_count(direction) + 1;
        self.trade_count_by_direction
            .insert(direction.to_string(), next);
    }

    fn add_shares(&mut self, condition_id: &str, shares: f64) {
        let entry = self
            .shares_by_condition
            .entry(condition_id.to_string())
            .or_insert(0.0);
        *entry += shares;
    }

    fn add_effective_cost(&mut self, condition_id: &str, amount: f64) {
        let entry = self
            .total_effective_cost_by_condition
            .entry(condition_id.to_string())
            .or_insert(0.0);
        *entry += amount;
    }

    fn avg_price(&self, condition_id: &str) -> Option<f64> {
        let shares = *self.shares_by_condition.get(condition_id).unwrap_or(&0.0);
        if shares <= 0.0 {
            return None;
        }
        let total_cost = *self
            .total_effective_cost_by_condition
            .get(condition_id)
            .unwrap_or(&0.0);
        Some(total_cost / shares)
    }

    fn total_eth_shares(&self, condition_id: &str) -> f64 {
        *self.shares_by_condition.get(condition_id).unwrap_or(&0.0)
    }

    fn total_btc_shares(&self, condition_id: &str) -> f64 {
        *self.shares_by_condition.get(condition_id).unwrap_or(&0.0)
    }
}

#[derive(Debug, Clone)]
struct RebalancePlan {
    side_condition_id: String,
    token_id: String,
    limit_price: f64,
    shares: f64,
}

pub struct Trader {
    api: Arc<PolymarketClient>,
    clob: Option<Arc<ClobClient>>,
    executor: Option<ExecutorClient>,
    config: TradingConfig,
    wallet: WalletConfig,
    signer: Option<WalletSigner>,
    sizing: PositionSizing,
    live_usdc_balance: Arc<Mutex<Decimal>>,
    execution_state: Arc<Mutex<ExecutionState>>,
}

impl Trader {
    fn window_key(opportunity: &ArbitrageOpportunity) -> String {
        let mut ids = vec![
            opportunity.eth_condition_id.clone(),
            opportunity.btc_condition_id.clone(),
        ];
        ids.sort();
        ids.join("|")
    }

    fn update_filled_state(
        state: &mut ExecutionState,
        condition_id: &str,
        shares: f64,
        effective_price_per_share: f64,
    ) {
        state.add_shares(condition_id, shares);
        state.add_effective_cost(condition_id, shares * effective_price_per_share);
    }

    fn rebalance_plan(
        &self,
        state: &ExecutionState,
        opportunity: &ArbitrageOpportunity,
        available_balance: f64,
    ) -> Option<RebalancePlan> {
        let eth_shares = state.total_eth_shares(&opportunity.eth_condition_id);
        let btc_shares = state.total_btc_shares(&opportunity.btc_condition_id);
        let imbalance = (eth_shares - btc_shares).abs();
        let min_shares = min_shares_from_env();
        if imbalance < min_shares {
            return None;
        }

        let (
            target_condition_id,
            token_id,
            raw_price,
            leg_liquidity,
            current_shares,
            other_shares,
            other_avg,
        ) = if eth_shares < btc_shares {
            (
                opportunity.eth_condition_id.clone(),
                opportunity.eth_up_token_id.clone(),
                opportunity.eth_up_price.to_f64().unwrap_or_default(),
                opportunity.eth_leg_ask_size.to_f64().unwrap_or_default(),
                eth_shares,
                btc_shares,
                state.avg_price(&opportunity.btc_condition_id),
            )
        } else {
            (
                opportunity.btc_condition_id.clone(),
                opportunity.btc_down_token_id.clone(),
                opportunity.btc_down_price.to_f64().unwrap_or_default(),
                opportunity.btc_leg_ask_size.to_f64().unwrap_or_default(),
                btc_shares,
                eth_shares,
                state.avg_price(&opportunity.eth_condition_id),
            )
        };

        let shares_to_add = imbalance.floor();
        if shares_to_add < min_shares {
            return None;
        }

        if leg_liquidity < shares_to_add {
            warn!(
                "🚫 REBALANCE SKIP | reason=insufficient_liquidity need={:.2} available={:.2}",
                shares_to_add, leg_liquidity
            );
            return None;
        }

        if let Some(cap) = max_total_shares_per_market_from_env() {
            if current_shares + shares_to_add > cap || other_shares > cap {
                warn!(
                    "🚫 REBALANCE SKIP | reason=exposure_cap projected={:.2} cap={:.2}",
                    current_shares + shares_to_add,
                    cap
                );
                return None;
            }
        }

        let effective_new_leg_price = effective_buy_price(raw_price);
        let projected_spend = shares_to_add * effective_new_leg_price;
        if projected_spend > available_balance {
            warn!(
                "🚫 REBALANCE SKIP | reason=insufficient_balance required=${:.4} balance=${:.4}",
                projected_spend, available_balance
            );
            return None;
        }

        let current_avg = state
            .avg_price(&target_condition_id)
            .unwrap_or(effective_new_leg_price);
        let projected_avg =
            ((current_avg * current_shares) + projected_spend) / (current_shares + shares_to_add);
        let current_total_avg = state
            .avg_price(&opportunity.eth_condition_id)
            .unwrap_or(0.0)
            + state
                .avg_price(&opportunity.btc_condition_id)
                .unwrap_or(0.0);
        let projected_total_avg = if target_condition_id == opportunity.eth_condition_id {
            projected_avg + other_avg.unwrap_or(0.0)
        } else {
            other_avg.unwrap_or(0.0) + projected_avg
        };

        let max_allowed = arbitrage_max_sum_from_env() + arbitrage_sum_tolerance_from_env();
        if projected_total_avg > max_allowed {
            warn!(
                "🚫 REBALANCE SKIP | reason=arb_ceiling projected_total={:.4} max_allowed={:.4}",
                projected_total_avg, max_allowed
            );
            return None;
        }

        if current_total_avg > 0.0 && projected_total_avg > current_total_avg {
            warn!(
                "🚫 REBALANCE SKIP | reason=worse_than_current projected_total={:.4} current_total={:.4}",
                projected_total_avg, current_total_avg
            );
            return None;
        }

        Some(RebalancePlan {
            side_condition_id: target_condition_id,
            token_id,
            limit_price: raw_price,
            shares: shares_to_add,
        })
    }

    pub fn new(
        api: Arc<PolymarketClient>,
        clob: Option<Arc<ClobClient>>,
        executor: Option<ExecutorClient>,
        config: TradingConfig,
        wallet: WalletConfig,
        signer: Option<WalletSigner>,
    ) -> Self {
        Self {
            api,
            clob,
            executor,
            config,
            wallet,
            signer,
            sizing: PositionSizing::from_env(),
            live_usdc_balance: Arc::new(Mutex::new(Decimal::ZERO)),
            execution_state: Arc::new(Mutex::new(ExecutionState::default())),
        }
    }

    async fn refresh_balance(&self) -> Result<()> {
        let bal = self.api.get_usdc_balance().await?;
        *self.live_usdc_balance.lock().await = bal;
        info!("💰 USDC balance: {}", bal);
        Ok(())
    }

    pub async fn execute_arbitrage(&self, opportunity: &ArbitrageOpportunity) -> Result<()> {
        self.refresh_balance().await?;

        let window_key = Self::window_key(opportunity);
        let trade_limit = per_direction_trade_limit_from_env();
        let balance = self.live_usdc_balance.lock().await.to_f64().unwrap_or(0.0);

        let (direction_count, rebalance_plan) = {
            let mut state = self.execution_state.lock().await;
            state.reset_for_window(window_key);

            let direction_count = state.direction_count(&opportunity.pair_label);
            let eth_shares = state.total_eth_shares(&opportunity.eth_condition_id);
            let btc_shares = state.total_btc_shares(&opportunity.btc_condition_id);
            info!(
                "📌 POSITION TRACKER | pair={} eth_shares={:.2} btc_shares={:.2} imbalance={:.2} direction_count={}",
                opportunity.pair_label,
                eth_shares,
                btc_shares,
                eth_shares - btc_shares,
                fmt_count(trade_limit, direction_count)
            );

            let plan = self.rebalance_plan(&state, opportunity, balance);
            (direction_count, plan)
        };

        let mut rebalance_only = false;
        let mut rebalance_target_condition = String::new();
        let mut effective_pair_cost = opportunity
            .effective_total_cost
            .to_f64()
            .unwrap_or_default();

        let mut units = if let Some(plan) = &rebalance_plan {
            rebalance_only = true;
            rebalance_target_condition = plan.side_condition_id.clone();
            info!(
                "⚖️ REBALANCE MODE | pair={} target_condition={} shares={:.2} token={}",
                opportunity.pair_label,
                plan.side_condition_id,
                plan.shares,
                &plan.token_id[..12.min(plan.token_id.len())]
            );
            effective_pair_cost = effective_buy_price(plan.limit_price);
            plan.shares
        } else {
            if trade_limit > 0 && direction_count >= trade_limit {
                warn!(
                    "🚫 TRADE SKIP | pair={} reason=direction_limit_reached count={} limit={}",
                    opportunity.pair_label, direction_count, trade_limit
                );
                return Ok(());
            }
            self.calculate_position_size(opportunity).await?
        };

        if units <= 0.0 {
            info!(
                "🚫 TRADE SKIP | pair={} reason=insufficient_units_after_sizing",
                opportunity.pair_label
            );
            return Ok(());
        }

        let min_shares = min_shares_from_env();
        let raw_pair_cost = opportunity.total_cost.to_f64().unwrap_or(0.0);
        if raw_pair_cost <= 0.0 {
            warn!(
                "🚫 TRADE SKIP | pair={} reason=invalid_total_cost total={}",
                opportunity.pair_label, raw_pair_cost
            );
            return Ok(());
        }

        let cost = if rebalance_only {
            effective_pair_cost
        } else {
            opportunity
                .effective_total_cost
                .to_f64()
                .unwrap_or(effective_pair_cost)
        };

        info!(
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🧾 TRADE INTENT
  pair={}
  mode={}
  total_cost_raw={:.4}
  total_cost_effective={:.4}
  expected_profit_effective={:.2}%
  arb_max_sum={:.4}
  arb_tolerance={:.4}
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━",
            opportunity.pair_label,
            if rebalance_only {
                "REBALANCE_ONLY"
            } else {
                "PAIRED"
            },
            raw_pair_cost,
            cost,
            opportunity.expected_profit.to_f64().unwrap_or_default() * 100.0,
            arbitrage_max_sum_from_env(),
            arbitrage_sum_tolerance_from_env(),
        );

        let liquidity_cap = if rebalance_only {
            if rebalance_target_condition == opportunity.eth_condition_id {
                opportunity.eth_leg_ask_size.to_f64().unwrap_or(0.0)
            } else {
                opportunity.btc_leg_ask_size.to_f64().unwrap_or(0.0)
            }
        } else {
            opportunity.max_shares.to_f64().unwrap_or(0.0)
        };
        let max_shares_cap = max_shares_cap_from_env();
        info!(
            "📏 Share bounds | MIN_SHARES={:.2} MAX_SHARES={} STRICT_SHARE_BOUNDS={}",
            min_shares,
            max_shares_cap
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "unset".to_string()),
            strict_share_bounds_from_env()
        );

        if units < min_shares && auto_bump_min_shares_from_env() {
            let required_for_min = min_shares * cost;
            let liquidity_allows = liquidity_cap >= min_shares;
            let max_cap_allows = max_shares_cap.map(|cap| cap >= min_shares).unwrap_or(true);

            if balance >= required_for_min && liquidity_allows && max_cap_allows {
                info!(
                    "🧩 Auto-bumping shares to MIN_SHARES because affordable and liquid: {:.2} -> {:.2}",
                    units, min_shares
                );
                units = min_shares;
            }
        }

        if units < min_shares {
            warn!(
                "❌ Trade skipped (minimum shares not met): shares={:.2} < {:.2}",
                units, min_shares
            );
            return Ok(());
        }

        if liquidity_cap < min_shares {
            warn!(
                "❌ Trade skipped (ask liquidity too low): max_shares={:.2} < {:.2}",
                liquidity_cap, min_shares
            );
            return Ok(());
        }

        if units > liquidity_cap {
            units = liquidity_cap.floor();
            info!("⚖️ Capped units by ask liquidity: units={}", units);
        }

        if let Some(max_shares_cap) = max_shares_cap {
            if units > max_shares_cap {
                units = max_shares_cap.floor();
                info!("⚖️ Capped units by MAX_SHARES: units={}", units);
            }

            if strict_share_bounds_from_env() && (max_shares_cap - min_shares).abs() < f64::EPSILON
            {
                if units >= max_shares_cap {
                    units = max_shares_cap;
                    info!(
                        "📌 Enforcing exact fixed share count because MIN_SHARES == MAX_SHARES: units={}",
                        units
                    );
                } else {
                    warn!(
                        "❌ Trade skipped (strict fixed shares): computed units={:.2}, required exactly {:.2}",
                        units, max_shares_cap
                    );
                    return Ok(());
                }
            }
        }

        if units < min_shares {
            warn!(
                "❌ Trade skipped after caps (minimum shares not met): shares={:.2} < {:.2}",
                units, min_shares
            );
            return Ok(());
        }

        let mut spend = units * cost;

        if spend > balance {
            let capped_units = (balance / cost).floor();
            if capped_units <= 0.0 {
                warn!("❌ Trade skipped (insufficient USDC for one unit)");
                return Ok(());
            }
            units = capped_units;
            spend = units * cost;
            info!(
                "⚖️ Capped units by balance: units={} total_spend=${:.2} balance=${:.2}",
                units, spend, balance
            );
        }

        let min_total_spend = Config::min_trade_size();
        if spend < min_total_spend {
            warn!(
                "❌ Trade skipped (below minimum total spend ${:.2})",
                min_total_spend
            );
            return Ok(());
        }

        if let Some(executor) = &self.executor {
            executor.healthcheck().await?;
            info!("🚀 EXECUTOR MODE | units={} spend=${:.2}", units, spend);

            let size_shares = Decimal::from_f64(units)
                .unwrap_or(Decimal::ZERO)
                .to_f64()
                .unwrap_or(0.0);

            if rebalance_only {
                let plan = rebalance_plan.ok_or_else(|| anyhow!("rebalance plan missing"))?;
                let resp = executor
                    .execute_order(&plan.token_id, Side::Buy, plan.limit_price, size_shares)
                    .await?;
                info!("✅ REBALANCE BUY submitted: {:?}", resp.order_id);

                let mut state = self.execution_state.lock().await;
                Self::update_filled_state(
                    &mut state,
                    &plan.side_condition_id,
                    units,
                    effective_buy_price(plan.limit_price),
                );
                info!(
                    "✅ REBALANCE OK | condition={} added_shares={:.2}",
                    plan.side_condition_id, units
                );
                return Ok(());
            }

            info!("🧮 Executor order sizing | shares_per_leg={}", units);
            info!(
                "🛡️ Execution safeguards | FOK={} ALLOW_PARTIAL_ARB={} RETRIES={}",
                executor.fok_enabled(),
                executor.allow_partial_arb(),
                executor_retry_attempts_from_env()
            );

            let attempts = executor_retry_attempts_from_env();
            let mut eth_resp = None;
            let mut btc_resp = None;

            for attempt in 1..=attempts {
                info!("🔁 Executor paired attempt {}/{}", attempt, attempts);

                let max_allowed = arbitrage_max_sum_from_env() + arbitrage_sum_tolerance_from_env();
                let live_check = tokio::try_join!(
                    crate::execution::orderbook::fetch_orderbook(
                        &self.api,
                        &opportunity.eth_up_token_id
                    ),
                    crate::execution::orderbook::fetch_orderbook(
                        &self.api,
                        &opportunity.btc_down_token_id
                    ),
                );

                if let Ok((eth_book, btc_book)) = live_check {
                    if let (Some((eth_ask, _)), Some((btc_ask, _))) =
                        (eth_book.best_ask(), btc_book.best_ask())
                    {
                        let effective_now =
                            effective_buy_price(eth_ask) + effective_buy_price(btc_ask);
                        if effective_now > max_allowed {
                            warn!(
                                "🚫 ATOMIC SKIP | reason=preflight_recheck_failed effective_total={:.4} max_allowed={:.4}",
                                effective_now, max_allowed
                            );
                            break;
                        }
                    }
                }

                let eth_future = executor.execute_order(
                    &opportunity.eth_up_token_id,
                    Side::Buy,
                    opportunity.eth_up_price.to_f64().unwrap_or_default(),
                    size_shares,
                );
                let btc_future = executor.execute_order(
                    &opportunity.btc_down_token_id,
                    Side::Buy,
                    opportunity.btc_down_price.to_f64().unwrap_or_default(),
                    size_shares,
                );

                let (eth_result, btc_result) = tokio::join!(eth_future, btc_future);

                match (eth_result, btc_result) {
                    (Ok(eth_ok), Ok(btc_ok)) => {
                        info!("✅ ETH leg submitted via executor: {:?}", eth_ok.order_id);
                        info!("✅ BTC leg submitted via executor: {:?}", btc_ok.order_id);
                        eth_resp = Some(eth_ok);
                        btc_resp = Some(btc_ok);
                        break;
                    }
                    (Ok(eth_ok), Err(e)) => {
                        eth_resp = Some(eth_ok);
                        warn!(
                            "❌ BTC leg failed on paired attempt {}/{}: {}",
                            attempt, attempts, e
                        );

                        let price_bump = second_leg_price_bump_cents_from_env() / 100.0;
                        let bumped_price =
                            (opportunity.btc_down_price.to_f64().unwrap_or_default() + price_bump)
                                .min(0.999999);
                        if bumped_price > opportunity.btc_down_price.to_f64().unwrap_or_default() {
                            let bumped_effective = effective_buy_price(
                                opportunity.eth_up_price.to_f64().unwrap_or_default(),
                            ) + effective_buy_price(bumped_price);
                            if bumped_effective <= max_allowed {
                                info!(
                                    "⚡ Retrying unfilled second leg with bounded price bump: {:.4} -> {:.4}",
                                    opportunity.btc_down_price,
                                    bumped_price
                                );
                                match executor
                                    .execute_order(
                                        &opportunity.btc_down_token_id,
                                        Side::Buy,
                                        bumped_price,
                                        size_shares,
                                    )
                                    .await
                                {
                                    Ok(resp) => {
                                        info!(
                                            "✅ BTC leg submitted via executor after bounded bump: {:?}",
                                            resp.order_id
                                        );
                                        btc_resp = Some(resp);
                                        break;
                                    }
                                    Err(bumped_err) => {
                                        warn!("❌ BTC bumped-price retry failed: {}", bumped_err);
                                    }
                                }
                            }
                        }

                        let unwind_price = opportunity.eth_up_bid_price.to_f64().unwrap_or(0.0);
                        if unwind_price > 0.0 {
                            match executor
                                .execute_order(
                                    &opportunity.eth_up_token_id,
                                    Side::Sell,
                                    unwind_price,
                                    size_shares,
                                )
                                .await
                            {
                                Ok(resp) => {
                                    warn!(
                                        "🧯 Hedged ETH leg immediately after BTC failure (sell order_id={:?})",
                                        resp.order_id
                                    );
                                }
                                Err(unwind_err) => {
                                    return Err(anyhow!(
                                        "second leg failed and immediate ETH unwind failed. second_leg_error: {}; unwind_error: {}",
                                        e,
                                        unwind_err
                                    ));
                                }
                            }
                        }

                        if attempt == attempts {
                            if executor.allow_partial_arb() {
                                warn!(
                                    "⚠️ BTC leg failed in all attempts, but ALLOW_PARTIAL_ARB=true so bot continues after unwind"
                                );
                                break;
                            }

                            return Err(anyhow!(
                                "second leg failed after {} attempts; first leg was unwound. error: {}",
                                attempts,
                                e
                            ));
                        }
                    }
                    (Err(e), Ok(btc_ok)) => {
                        btc_resp = Some(btc_ok);
                        warn!(
                            "❌ ETH leg failed on paired attempt {}/{}: {}",
                            attempt, attempts, e
                        );

                        let unwind_price = opportunity.btc_down_bid_price.to_f64().unwrap_or(0.0);
                        if unwind_price > 0.0 {
                            let _ = executor
                                .execute_order(
                                    &opportunity.btc_down_token_id,
                                    Side::Sell,
                                    unwind_price,
                                    size_shares,
                                )
                                .await;
                        }

                        if attempt == attempts && !executor.allow_partial_arb() {
                            return Err(anyhow!(
                                "first leg failed after {} attempts; second leg was unwound. error: {}",
                                attempts,
                                e
                            ));
                        }
                    }
                    (Err(eth_err), Err(btc_err)) => {
                        warn!(
                            "❌ Both legs failed on paired attempt {}/{}: eth={} | btc={}",
                            attempt, attempts, eth_err, btc_err
                        );
                        if attempt == attempts && !executor.allow_partial_arb() {
                            return Err(anyhow!(
                                "both legs failed after {} attempts: eth={} | btc={}",
                                attempts,
                                eth_err,
                                btc_err
                            ));
                        }
                    }
                }
            }

            if eth_resp.is_none() || btc_resp.is_none() {
                if !executor.allow_partial_arb() {
                    return Err(anyhow!(
                        "paired execution did not complete both legs successfully"
                    ));
                }
            } else {
                let mut state = self.execution_state.lock().await;
                Self::update_filled_state(
                    &mut state,
                    &opportunity.eth_condition_id,
                    units,
                    effective_buy_price(opportunity.eth_up_price.to_f64().unwrap_or_default()),
                );
                Self::update_filled_state(
                    &mut state,
                    &opportunity.btc_condition_id,
                    units,
                    effective_buy_price(opportunity.btc_down_price.to_f64().unwrap_or_default()),
                );
                state.increment_direction(&opportunity.pair_label);
                info!(
                    "✅ TRADE OK | pair={} shares_per_leg={:.2} direction_count={}",
                    opportunity.pair_label,
                    units,
                    fmt_count(
                        per_direction_trade_limit_from_env(),
                        state.direction_count(&opportunity.pair_label)
                    )
                );
            }

            return Ok(());
        }

        let clob = self
            .clob
            .as_ref()
            .ok_or_else(|| anyhow!("direct CLOB mode requires clob client"))?;

        clob.ensure_trading_ready((spend * 1_000_000.0) as u128)
            .await?;

        let size_dec = Decimal::from_f64(units).unwrap_or(Decimal::ZERO);
        if rebalance_only {
            let plan = rebalance_plan.ok_or_else(|| anyhow!("rebalance plan missing"))?;
            self.place_leg(
                &plan.token_id,
                0,
                Decimal::from_f64(plan.limit_price).unwrap_or(Decimal::ZERO),
                size_dec,
            )
            .await?;

            let mut state = self.execution_state.lock().await;
            Self::update_filled_state(
                &mut state,
                &plan.side_condition_id,
                units,
                effective_buy_price(plan.limit_price),
            );
            info!(
                "✅ REBALANCE OK | condition={} added_shares={:.2}",
                plan.side_condition_id, units
            );
            return Ok(());
        }

        self.place_leg(
            &opportunity.eth_up_token_id,
            0,
            opportunity.eth_up_price,
            size_dec,
        )
        .await?;

        self.place_leg(
            &opportunity.btc_down_token_id,
            0,
            opportunity.btc_down_price,
            size_dec,
        )
        .await?;

        {
            let mut state = self.execution_state.lock().await;
            Self::update_filled_state(
                &mut state,
                &opportunity.eth_condition_id,
                units,
                effective_buy_price(opportunity.eth_up_price.to_f64().unwrap_or_default()),
            );
            Self::update_filled_state(
                &mut state,
                &opportunity.btc_condition_id,
                units,
                effective_buy_price(opportunity.btc_down_price.to_f64().unwrap_or_default()),
            );
            state.increment_direction(&opportunity.pair_label);
            info!(
                "✅ TRADE OK | pair={} shares_per_leg={:.2} direction_count={}",
                opportunity.pair_label,
                units,
                fmt_count(
                    per_direction_trade_limit_from_env(),
                    state.direction_count(&opportunity.pair_label)
                )
            );
        }

        Ok(())
    }

    async fn place_leg(
        &self,
        token_id: &str,
        side: u8,
        price: Decimal,
        size: Decimal,
    ) -> Result<()> {
        let clob = self
            .clob
            .as_ref()
            .ok_or_else(|| anyhow!("direct CLOB mode requires clob client"))?;
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| anyhow!("direct CLOB mode requires signer"))?;

        let price_u256 = to_u256_scaled(price);
        let size_u256 = to_u256_scaled(size);

        let (maker_amount, taker_amount) = if side == 0 {
            (price_u256 * size_u256 / U256::from(1_000_000), size_u256)
        } else {
            (size_u256, price_u256 * size_u256 / U256::from(1_000_000))
        };

        let order = ClobOrder {
            salt: U256::from(::rand::random::<u64>()),
            maker: Address::from_str(&self.wallet.proxy_wallet)?,
            signer: signer.address(),
            taker: Address::zero(),
            token_id: str_to_h256(token_id),
            maker_amount,
            taker_amount,
            side,
            fee_rate_bps: U256::zero(),
            nonce: make_nonce(),
            expiration: U256::from(now_ts() + 300),
        };

        let sig = signer.sign_order(&order).await?;

        match clob
            .submit_order(order, sig, &self.wallet.proxy_wallet)
            .await
        {
            Ok(_) => info!("✅ Order submitted {}", token_id),
            Err(e) => warn!("❌ Order rejected {} → {}", token_id, e),
        }

        Ok(())
    }

    async fn calculate_position_size(&self, opportunity: &ArbitrageOpportunity) -> Result<f64> {
        let _check_interval_ms = self.config.check_interval_ms;
        let bal = self.live_usdc_balance.lock().await;
        let balance = bal.to_f64().unwrap_or(0.0);
        let cost = opportunity.total_cost.to_f64().unwrap_or(1.0);

        let mode_name = match self.sizing.mode {
            TradeMode::Fixed => "FIXED",
            TradeMode::Percentage => "PERCENTAGE",
            TradeMode::Dynamic => "DYNAMIC",
            TradeMode::Free => "FREE",
        };

        let spend = match self.sizing.mode {
            TradeMode::Fixed => self.sizing.fixed_usdc.unwrap_or(0.0),
            TradeMode::Percentage => balance * (self.sizing.percentage.unwrap_or(10.0) / 100.0),
            TradeMode::Dynamic => {
                let edge = opportunity.expected_profit.to_f64().unwrap_or(0.0);
                (balance * 0.01 * (1.0 + edge)).min(balance * 0.25)
            }
            TradeMode::Free => balance,
        };

        let raw_units = spend / cost;
        let units = raw_units.floor();

        info!(
            "🧮 Position sizing | mode={} balance=${:.6} spend=${:.6} cost_per_pair=${:.6} raw_units={:.4} floored_units={:.0}",
            mode_name, balance, spend, cost, raw_units, units
        );

        Ok(units)
    }
}
