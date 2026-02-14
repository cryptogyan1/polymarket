use chrono::{DateTime, Utc};

pub fn time_remaining(end: DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = end - now;

    if diff.num_seconds() <= 0 {
        return "CLOSED".to_string();
    }

    let mins = diff.num_minutes();
    let secs = diff.num_seconds() % 60;

    format!("{:02}m {:02}s", mins, secs)
}
