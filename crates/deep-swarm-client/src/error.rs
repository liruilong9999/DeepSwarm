use reqwest::StatusCode;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid request ({status}): {message}")]
    InvalidRequest { status: u16, message: String },
    #[error("authentication failed: {0}")]
    Authentication(String),
    #[error("insufficient balance: {0}")]
    InsufficientBalance(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("server error ({status}): {message}")]
    ServerError { status: u16, message: String },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("request timed out: {0}")]
    Timeout(String),
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl Error {
    pub fn from_status(status: StatusCode, message: impl Into<String>) -> Self {
        let message = message.into();
        match status.as_u16() {
            401 => Self::Authentication(message),
            402 => Self::InsufficientBalance(message),
            429 => Self::RateLimited(message),
            500..=599 => Self::ServerError {
                status: status.as_u16(),
                message,
            },
            _ => Self::InvalidRequest {
                status: status.as_u16(),
                message,
            },
        }
    }

    pub(crate) fn from_reqwest(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout(error.to_string())
        } else {
            Self::Transport(error.to_string())
        }
    }

    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited(_)
                | Self::ServerError {
                    status: 500 | 503,
                    ..
                }
                | Self::Transport(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use reqwest::StatusCode;

    #[test]
    fn maps_every_documented_http_status() {
        assert!(matches!(
            Error::from_status(StatusCode::BAD_REQUEST, "bad"),
            Error::InvalidRequest { .. }
        ));
        assert!(matches!(
            Error::from_status(StatusCode::UNAUTHORIZED, "bad"),
            Error::Authentication(_)
        ));
        assert!(matches!(
            Error::from_status(StatusCode::PAYMENT_REQUIRED, "bad"),
            Error::InsufficientBalance(_)
        ));
        assert!(matches!(
            Error::from_status(StatusCode::NOT_FOUND, "bad"),
            Error::InvalidRequest { .. }
        ));
        assert!(matches!(
            Error::from_status(StatusCode::UNPROCESSABLE_ENTITY, "bad"),
            Error::InvalidRequest { .. }
        ));
        assert!(matches!(
            Error::from_status(StatusCode::TOO_MANY_REQUESTS, "bad"),
            Error::RateLimited(_)
        ));
        assert!(matches!(
            Error::from_status(StatusCode::INTERNAL_SERVER_ERROR, "bad"),
            Error::ServerError { status: 500, .. }
        ));
        assert!(matches!(
            Error::from_status(StatusCode::SERVICE_UNAVAILABLE, "bad"),
            Error::ServerError { status: 503, .. }
        ));
        assert!(!Error::from_status(StatusCode::BAD_GATEWAY, "bad").is_retryable());
    }
}
