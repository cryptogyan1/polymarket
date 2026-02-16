use anyhow::{anyhow, Result};
use chrono::Utc;
use log::{error, info, warn, LevelFilter, Log, Metadata, Record};
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

struct DualLogger {
    level: LevelFilter,
    file: Mutex<std::fs::File>,
}

impl Log for DualLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        println!("{}", record.args());

        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(
                file,
                "[{} {} {}] {}",
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

static LOGGER: OnceLock<DualLogger> = OnceLock::new();

pub fn init_logging() -> Result<()> {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|raw| raw.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::Info);

    let log_path = std::env::var("LOG_FILE").unwrap_or_else(|_| "logs/bot.log".to_string());
    let path = Path::new(&log_path);

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_all(parent)?;
        }
    }

    let file = OpenOptions::new().create(true).append(true).open(path)?;

    let logger = LOGGER.get_or_init(|| DualLogger {
        level,
        file: Mutex::new(file),
    });

    log::set_logger(logger).map_err(|e| anyhow!("failed to initialize logger: {e}"))?;
    log::set_max_level(level);

    Ok(())
}

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
