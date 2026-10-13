use models_email::email::service::link::UserProvider;
use std::fmt;

use thiserror::Error;

/// An opaque bearer token used to authorize provider API requests.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    secret: String,
    provider: UserProvider,
}

impl AccessToken {
    /// Wraps an access token without validating provider-specific syntax.
    pub fn new(token: impl Into<String>) -> Self {
        Self::for_provider(token, UserProvider::Gmail)
    }

    /// Wraps a token issued for the linked mailbox's provider.
    pub fn for_provider(token: impl Into<String>, provider: UserProvider) -> Self {
        Self {
            secret: token.into(),
            provider,
        }
    }

    /// Provider that issued this token.
    pub fn provider(&self) -> UserProvider {
        self.provider
    }

    /// Borrows the token for an outbound provider adapter.
    pub fn expose_secret(&self) -> &str {
        &self.secret
    }

    /// Consumes this value and returns the token string.
    pub fn into_secret(self) -> String {
        self.secret
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken([REDACTED])")
    }
}

impl From<String> for AccessToken {
    fn from(token: String) -> Self {
        Self::new(token)
    }
}

/// Controls whether a token may come from a cache or must be refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenFreshness {
    /// A valid cached token may be returned.
    #[default]
    Cached,
    /// The provider grant must be checked by fetching a fresh token.
    Fresh,
}

/// A provider-neutral token acquisition failure.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum TokenError {
    /// The grant is absent or revoked and the user must reconnect it.
    #[error("email provider authorization is required")]
    ReauthRequired,
    /// Token acquisition failed temporarily and may be retried.
    #[error("access token acquisition failed temporarily: {message}")]
    Transient {
        /// A sanitized diagnostic suitable for logs.
        message: String,
    },
    /// Token acquisition failed permanently for a reason other than reauthorization.
    #[error("access token acquisition failed: {message}")]
    Permanent {
        /// A sanitized diagnostic suitable for logs.
        message: String,
    },
}

#[cfg(test)]
mod test;
