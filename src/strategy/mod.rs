use crate::domain::*;
use crate::monitor::MarketSnapshot;
use log::{debug, info};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::env;

#[derive(Clone)]
pub struct ArbitrageDetector {
    min_profit_threshold: Decimal,
    max_sum_threshold: Decimal,
    max_sum_tolerance: Decimal,
    min_reasonable_price: Decimal,
    max_reasonable_price: Decimal,
    min_total_cost: Decimal,
    min_shares: Decimal,
    fee_bps: Decimal,
    slippage_bps: Decimal,
}

impl ArbitrageDetector {
    pub fn new(min_profit_threshold: f64) -> Self {
        // Read ARBITRAGE_MAX_SUM from env (default: 0.99)
        let max_sum = env::var("ARBITRAGE_MAX_SUM")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.978);

        // Read MIN_REASONABLE_PRICE from env (default: 0.15)
        let min_reasonable = env::var("MIN_REASONABLE_PRICE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.15);

        // Read MAX_REASONABLE_PRICE from env (default: 0.95)
        let max_reasonable = env::var("MAX_REASONABLE_PRICE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.95);

        // Read MIN_TOTAL_COST from env (default: 0.50)
        let min_shares = env::var("MIN_SHARES")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(5.0);
        let min_total = env::var("MIN_TOTAL_COST")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.50);

        let max_sum_tolerance = env::var("ARBITRAGE_SUM_TOLERANCE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.02);

        let fee_bps = env::var("TRADE_FEE_BPS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(100.0);

        let slippage_bps = env::var("SLIPPAGE_BPS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(35.0);

        info!("🎯 Arbitrage Detector Initialized:");
        info!(
            "   Min profit threshold: {:.2}%",
            min_profit_threshold * 100.0
        );
        info!("   Max sum threshold: ${:.4}", max_sum);
        info!("   Min reasonable price: ${:.4}", min_reasonable);
        info!("   Max reasonable price: ${:.4}", max_reasonable);
        info!("   Min total cost: ${:.4}", min_total);
        info!("   Min shares required: {:.2}", min_shares);
        info!("   Max sum tolerance: ${:.4}", max_sum_tolerance);
        info!("   Fee bps: {:.2}", fee_bps);
        info!("   Slippage bps: {:.2}", slippage_bps);

        Self {
            min_profit_threshold: Decimal::from_f64(min_profit_threshold).unwrap_or(dec!(0.01)),
            max_sum_threshold: Decimal::from_f64(max_sum).unwrap_or(dec!(0.99)),
            max_sum_tolerance: Decimal::from_f64(max_sum_tolerance).unwrap_or(dec!(0.02)),
            min_reasonable_price: Decimal::from_f64(min_reasonable).unwrap_or(dec!(0.15)),
            max_reasonable_price: Decimal::from_f64(max_reasonable).unwrap_or(dec!(0.95)),
            min_total_cost: Decimal::from_f64(min_total).unwrap_or(dec!(0.50)),
            min_shares: Decimal::from_f64(min_shares).unwrap_or(dec!(5.0)),
            fee_bps: Decimal::from_f64(fee_bps).unwrap_or(dec!(100)),
            slippage_bps: Decimal::from_f64(slippage_bps).unwrap_or(dec!(35)),
        }
    }

    /// Core strategy:
    /// 1) ETH UP  + BTC DOWN
    /// 2) ETH DOWN + BTC UP
    ///
    /// Execute ONLY when total cost < max_sum_threshold and profit >= min_profit_threshold
    /// Apply safety filters to prevent rug/fake pricing
    pub fn detect_opportunities(&self, snapshot: &MarketSnapshot) -> Vec<ArbitrageOpportunity> {
        let mut opportunities = Vec::new();

        let eth_up = snapshot.eth_market.up_token.as_ref();
        let eth_down = snapshot.eth_market.down_token.as_ref();
        let btc_up = snapshot.btc_market.up_token.as_ref();
        let btc_down = snapshot.btc_market.down_token.as_ref();

        let left_name = snapshot.eth_market.market_name.to_uppercase();
        let right_name = snapshot.btc_market.market_name.to_uppercase();

        // ===============================
        // PAIR 1: LEFT UP + RIGHT DOWN
        // ===============================
        if let (Some(eth), Some(btc)) = (eth_up, btc_down) {
            let pair_1_label = format!("{}_UP + {}_DOWN", left_name, right_name);
            if let Some(o) = self.check_pair(
                eth,
                btc,
                &snapshot.eth_market.condition_id,
                &snapshot.btc_market.condition_id,
                &pair_1_label,
            ) {
                opportunities.push(o);
            }
        }

        // ===============================
        // PAIR 2: LEFT DOWN + RIGHT UP
        // ===============================
        if let (Some(eth), Some(btc)) = (eth_down, btc_up) {
            let pair_2_label = format!("{}_DOWN + {}_UP", left_name, right_name);
            if let Some(o) = self.check_pair(
                eth,
                btc,
                &snapshot.eth_market.condition_id,
                &snapshot.btc_market.condition_id,
                &pair_2_label,
            ) {
                opportunities.push(o);
            }
        }

        // ╔═══════════════════════════════════════════════════════════╗
        // ║  CHANGED SECTION - Lines 111-113 ADDED                   ║
        // ║  What: Added logging before returning opportunities      ║
        // ║  Why: Track flow from strategy to trader                 ║
        // ╚═══════════════════════════════════════════════════════════╝
        if !opportunities.is_empty() {
            info!(
                "🎯 Strategy returning {} opportunity(ies) to trader",
                opportunities.len()
            );
        }
        // ╔═══════════════════════════════════════════════════════════╗
        // ║  END OF CHANGED SECTION                                   ║
        // ╚═══════════════════════════════════════════════════════════╝

        opportunities
    }

    fn effective_buy_price(&self, ask: Decimal) -> Decimal {
        let fee_mult = dec!(1) + (self.fee_bps / dec!(10000));
        let slip_mult = dec!(1) + (self.slippage_bps / dec!(10000));
        ask * fee_mult * slip_mult
    }

    fn check_pair(
        &self,
        token_a: &TokenPrice,
        token_b: &TokenPrice,
        eth_condition_id: &str,
        btc_condition_id: &str,
        pair_label: &str,
    ) -> Option<ArbitrageOpportunity> {
        // BUY prices (what we pay)
        let price_a = token_a.ask?;
        let price_b = token_b.ask?;

        let ask_size_a = token_a.ask_size?;
        let ask_size_b = token_b.ask_size?;
        let max_shares = ask_size_a.min(ask_size_b);

        if max_shares < self.min_shares {
            info!(
                "   ❌ Rejected: ask liquidity too low for equal shares (max_shares={:.2} < min_shares={:.2})",
                max_shares, self.min_shares
            );
            return None;
        }

        debug!(
            "Checking pair: price_a={}, price_b={}, total={}",
            price_a,
            price_b,
            price_a + price_b
        );

        let total_cost = price_a + price_b;
        let effective_price_a = self.effective_buy_price(price_a);
        let effective_price_b = self.effective_buy_price(price_b);
        let effective_total_cost = effective_price_a + effective_price_b;

        // ===============================
        // SAFETY FILTER #1: Both prices too low (rug pricing)
        // User configurable via MIN_REASONABLE_PRICE
        // ===============================
        if price_a < self.min_reasonable_price && price_b < self.min_reasonable_price {
            info!(
                "   ❌ Rejected: Both prices (${:.4}, ${:.4}) < min_reasonable (${:.4})",
                price_a, price_b, self.min_reasonable_price
            );
            return None;
        }

        // ===============================
        // SAFETY FILTER #2: Both prices too high (no arb possible)
        // User configurable via MAX_REASONABLE_PRICE
        // ===============================
        if price_a > self.max_reasonable_price && price_b > self.max_reasonable_price {
            info!(
                "   ❌ Rejected: Both prices (${:.4}, ${:.4}) > max_reasonable (${:.4})",
                price_a, price_b, self.max_reasonable_price
            );
            return None;
        }

        // ===============================
        // SAFETY FILTER #3: Total cost suspiciously low
        // User configurable via MIN_TOTAL_COST
        // ===============================
        if total_cost < self.min_total_cost {
            info!(
                "   ❌ Rejected: Total cost ${:.4} < min_total_cost ${:.4}",
                total_cost, self.min_total_cost
            );
            return None;
        }

        // ===============================
        // ARBITRAGE CHECK: Total cost vs max threshold
        // User configurable via ARBITRAGE_MAX_SUM
        // ===============================
        let max_allowed = self.max_sum_threshold + self.max_sum_tolerance;
        if effective_total_cost > max_allowed {
            info!(
                "   ❌ Rejected: Effective total cost ${:.4} > max_allowed ${:.4} (raw_total=${:.4})",
                effective_total_cost, max_allowed, total_cost
            );
            return None;
        }

        // ===============================
        // PROFIT CHECK: Expected profit vs minimum threshold
        // User configurable via MIN_PROFIT_THRESHOLD
        // ===============================
        let expected_profit = dec!(1.0) - effective_total_cost;

        if expected_profit < self.min_profit_threshold {
            info!(
                "   ❌ Rejected: Expected profit ${:.4} ({:.2}%) < threshold ${:.4} ({:.2}%)",
                expected_profit,
                expected_profit.to_f64().unwrap() * 100.0,
                self.min_profit_threshold,
                self.min_profit_threshold.to_f64().unwrap() * 100.0
            );
            return None;
        }

        // ===============================
        // ✅ VALID ARBITRAGE OPPORTUNITY!
        // ===============================
        info!("   ✅ VALID ARBITRAGE FOUND! [{}]", pair_label);
        info!("      Price A: ${:.4}", price_a);
        info!("      Price B: ${:.4}", price_b);
        info!("      Total Cost (raw): ${:.4}", total_cost);
        info!("      Total Cost (effective): ${:.4}", effective_total_cost);
        info!(
            "      Expected Profit (effective): ${:.4} ({:.2}%)",
            expected_profit,
            expected_profit.to_f64().unwrap() * 100.0
        );

        Some(ArbitrageOpportunity {
            detected_at: std::time::Instant::now(),
            pair_label: pair_label.to_string(),
            eth_condition_id: eth_condition_id.to_string(),
            btc_condition_id: btc_condition_id.to_string(),

            // these are the two tokens we BUY
            eth_up_token_id: token_a.token_id.clone(),
            btc_down_token_id: token_b.token_id.clone(),

            eth_up_price: price_a,
            btc_down_price: price_b,
            eth_up_bid_price: token_a.bid.unwrap_or(dec!(0)),
            btc_down_bid_price: token_b.bid.unwrap_or(dec!(0)),

            total_cost,
            effective_total_cost,
            expected_profit,
            max_shares,
            eth_leg_ask_size: ask_size_a,
            btc_leg_ask_size: ask_size_b,
        })
    }
}
