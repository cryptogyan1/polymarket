use std::fmt;

#[derive(Debug)]
pub enum ExecutionError {
    Rejected(String),
    PartialFill { filled: f64, remaining: f64 },
    RetryLimitExceeded,
    InvalidSize,
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionError::Rejected(msg) => write!(f, "Order rejected: {}", msg),
            ExecutionError::PartialFill { filled, remaining } => {
                write!(
                    f,
                    "Partial fill: filled {}, remaining {}",
                    filled, remaining
                )
            }
            ExecutionError::RetryLimitExceeded => write!(f, "Retry limit exceeded"),
            ExecutionError::InvalidSize => write!(f, "Invalid order size"),
        }
    }
}

impl std::error::Error for ExecutionError {}
