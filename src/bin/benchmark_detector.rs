use polymarket_15m_arbitrage_bot::domain::{MarketData, TokenPrice};
use polymarket_15m_arbitrage_bot::monitor::MarketSnapshot;
use polymarket_15m_arbitrage_bot::strategy::ArbitrageDetector;
use rust_decimal_macros::dec;
use std::hint::black_box;
use std::time::Instant;

fn sample_snapshot() -> MarketSnapshot {
    MarketSnapshot {
        eth_market: MarketData {
            condition_id: "eth-cond".to_string(),
            market_name: "ETH".to_string(),
            up_token: Some(TokenPrice {
                token_id: "eth-up".to_string(),
                bid: Some(dec!(0.476)),
                ask: Some(dec!(0.480)),
                bid_size: Some(dec!(250)),
                ask_size: Some(dec!(240)),
            }),
            down_token: Some(TokenPrice {
                token_id: "eth-down".to_string(),
                bid: Some(dec!(0.510)),
                ask: Some(dec!(0.514)),
                bid_size: Some(dec!(220)),
                ask_size: Some(dec!(230)),
            }),
        },
        btc_market: MarketData {
            condition_id: "btc-cond".to_string(),
            market_name: "BTC".to_string(),
            up_token: Some(TokenPrice {
                token_id: "btc-up".to_string(),
                bid: Some(dec!(0.495)),
                ask: Some(dec!(0.499)),
                bid_size: Some(dec!(260)),
                ask_size: Some(dec!(255)),
            }),
            down_token: Some(TokenPrice {
                token_id: "btc-down".to_string(),
                bid: Some(dec!(0.472)),
                ask: Some(dec!(0.476)),
                bid_size: Some(dec!(245)),
                ask_size: Some(dec!(250)),
            }),
        },
        timestamp: Instant::now(),
    }
}

fn main() {
    let iters: u64 = std::env::var("BENCH_ITERS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(200_000);

    let detector = ArbitrageDetector::new(0.005);
    let snapshot = sample_snapshot();

    let warmup = 10_000u64.min(iters);
    let mut warm_count = 0usize;
    for _ in 0..warmup {
        warm_count += black_box(detector.detect_opportunities(&snapshot)).len();
    }

    let start = Instant::now();
    let mut opportunities_seen = 0usize;
    for _ in 0..iters {
        opportunities_seen += black_box(detector.detect_opportunities(&snapshot)).len();
    }
    let elapsed = start.elapsed();

    let total_ns = elapsed.as_nanos() as f64;
    let ns_per_iter = total_ns / iters as f64;
    let us_per_iter = ns_per_iter / 1_000.0;
    let iters_per_sec = iters as f64 / elapsed.as_secs_f64();

    println!("Detector benchmark complete");
    println!("  iterations: {}", iters);
    println!("  warmup iterations: {}", warmup);
    println!("  warmup opportunities observed: {}", warm_count);
    println!("  opportunities observed: {}", opportunities_seen);
    println!("  elapsed: {:.3?}", elapsed);
    println!("  avg latency: {:.2} µs/iteration", us_per_iter);
    println!("  throughput: {:.0} iterations/sec", iters_per_sec);
}
