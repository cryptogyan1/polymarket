use log::{error, info, warn};

pub fn log_rejection(reason: &str) {
    error!("❌ Rejected: {}", reason);
}

pub fn log_retry(attempt: u32, reason: &str) {
    warn!("🔁 Retry {} — {}", attempt, reason);
}

pub fn log_partial(filled: f64, remaining: f64) {
    warn!(
        "⚠️ Partial fill — filled ${:.2}, remaining ${:.2}",
        filled, remaining
    );
}

pub fn log_success(msg: &str) {
    info!("✅ {}", msg);
}
