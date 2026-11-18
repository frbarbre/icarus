use std::time::Duration;

pub type Result<T, E = GitHubError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum GitHubError {
    #[error("could not run gh: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("gh failed: {0}")]
    Gh(String),
    #[error("GitHub returned HTTP {status}: {message}")]
    Http {
        status: u16,
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("{}", .0.join("; "))]
    GraphQl(Vec<String>),
    #[error("GitHub returned a malformed response: {0}")]
    Malformed(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("GitHub stacked pull requests are not available for this repository.")]
    StacksUnavailable,
    #[error("Could not verify GitHub pull request stack metadata. Refresh and try again.")]
    StackMetadataUnavailable,
    #[error("{0}")]
    StackMerge(String),
    #[error("This pull request can already be merged. Use Merge instead of auto-merge.")]
    AlreadyMergeable,
    #[error("GitHub {resource} rate limit reached; waiting for it to reset")]
    RateLimited { resource: String },
}

impl GitHubError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}
