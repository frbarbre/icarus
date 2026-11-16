use std::time::Duration;

pub type Result<T, E = LinearError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum LinearError {
    #[error("Linear is not configured: paste an API key in Settings or run `icarus linear login`")]
    NotConfigured,
    #[error("Linear rejected the API key")]
    Unauthorized,
    #[error("Linear rate limit reached{}", retry_hint(.retry_after))]
    RateLimited { retry_after: Option<Duration> },
    #[error("Linear issue {0} not found")]
    NotFound(String),
    #[error("`{0}` is not a Linear issue identifier (expected e.g. ABC-12)")]
    InvalidIdentifier(String),
    #[error("Linear returned HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("Linear: {}", .0.join("; "))]
    GraphQl(Vec<String>),
    #[error("could not reach Linear: {0}")]
    Transport(#[from] std::io::Error),
    #[error("Linear returned a malformed response: {0}")]
    Malformed(String),
}

fn retry_hint(retry_after: &Option<Duration>) -> String {
    match retry_after {
        Some(after) => format!("; retry in {}s", after.as_secs().max(1)),
        None => String::new(),
    }
}

impl LinearError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited { .. } => "rate_limited",
            Self::NotFound(_) => "not_found",
            Self::InvalidIdentifier(_) => "invalid_params",
            Self::Http { .. } | Self::GraphQl(_) | Self::Transport(_) | Self::Malformed(_) => {
                "linear_failed"
            }
        }
    }
}
