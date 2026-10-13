//! Access-token adapter for user-initiated calendar mutations.

#[cfg(test)]
mod test;

use authentication_service_client::{AuthServiceClient, error::AuthServiceClientError};
use calendar_events::domain::{
    models::{CalendarLinkTokenIdentity, CalendarProvider, GoogleScopeSet},
    ports::{CalendarAccessTokenProvider, CalendarTokenError},
};
use email_utils::token_cache_key::TokenCacheKey;
use google_token::fetch_gmail_access_token;
use redis::aio::MultiplexedConnection;
use std::sync::Arc;

/// Mints a calendar-scoped access token for the configured provider.
#[derive(Clone)]
pub struct CalendarTokenProviderAdapter {
    redis_conn: MultiplexedConnection,
    auth_service_client: Arc<AuthServiceClient>,
    provider: CalendarProvider,
}

impl CalendarTokenProviderAdapter {
    /// Construct the adapter from the shared Redis connection and auth client.
    pub fn new(
        redis_conn: MultiplexedConnection,
        auth_service_client: Arc<AuthServiceClient>,
        provider: CalendarProvider,
    ) -> Self {
        Self {
            redis_conn,
            auth_service_client,
            provider,
        }
    }
}

impl CalendarAccessTokenProvider for CalendarTokenProviderAdapter {
    async fn fetch_access_token(
        &self,
        identity: &CalendarLinkTokenIdentity,
    ) -> Result<String, CalendarTokenError> {
        match self.provider {
            CalendarProvider::Google => {
                let key = TokenCacheKey::new(
                    &identity.fusionauth_user_id,
                    &identity.email_address,
                    &identity.provider,
                );
                fetch_gmail_access_token(&key, &self.redis_conn, &self.auth_service_client)
                    .await
                    .map_err(|error| {
                        let reauth = error.chain().any(|cause| {
                            cause
                                .downcast_ref::<AuthServiceClientError>()
                                .is_some_and(|cause| {
                                    matches!(
                                        cause,
                                        AuthServiceClientError::Forbidden
                                            | AuthServiceClientError::NotFound
                                    )
                                })
                        });
                        if reauth {
                            CalendarTokenError::ReauthRequired(format!("{error:?}"))
                        } else {
                            CalendarTokenError::Transient(format!("{error:?}"))
                        }
                    })
            }
            CalendarProvider::Stalwart => {
                fetch_stalwart_calendar_token(&self.auth_service_client, identity).await
            }
        }
    }
}

async fn fetch_stalwart_calendar_token(
    auth_service_client: &AuthServiceClient,
    identity: &CalendarLinkTokenIdentity,
) -> Result<String, CalendarTokenError> {
    let token = auth_service_client
        .get_stalwart_access_token(&identity.fusionauth_user_id, &identity.email_address)
        .await
        .map_err(|error| match error {
            AuthServiceClientError::Forbidden
            | AuthServiceClientError::NotFound
            | AuthServiceClientError::Unauthorized => {
                CalendarTokenError::ReauthRequired(error.to_string())
            }
            _ => CalendarTokenError::Transient(error.to_string()),
        })?;
    if !GoogleScopeSet::parse(token.scope.as_deref().unwrap_or_default())
        .has_calendar_capability_for(CalendarProvider::Stalwart)
    {
        return Err(CalendarTokenError::ReauthRequired(
            "Stalwart calendar scope is missing from the access token".to_string(),
        ));
    }
    Ok(token.access_token)
}
