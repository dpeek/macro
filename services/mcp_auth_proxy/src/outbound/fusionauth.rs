//! FusionAuth adapter for the MCP OAuth broker.

use anyhow::Context;
use std::sync::Arc;
use tracing::Instrument;

use crate::domain::{
    models::{RefreshToken, UpstreamTokens},
    ports::{OAuthProvider, UpstreamTokensFuture},
};

/// FusionAuth-backed OAuth provider for the MCP auth proxy.
#[derive(Clone)]
pub struct FusionAuthOAuthProvider {
    client: Arc<fusionauth::FusionAuthClient>,
    identity_provider_id: String,
}

impl FusionAuthOAuthProvider {
    /// Creates a provider and resolves its FusionAuth identity provider ID.
    #[tracing::instrument(skip(client), err)]
    pub async fn new(
        client: fusionauth::FusionAuthClient,
        identity_provider_name: &str,
    ) -> anyhow::Result<Self> {
        let identity_provider_id = client
            .get_identity_provider_id_by_name(identity_provider_name)
            .await
            .with_context(|| {
                format!(
                    "failed to look up {identity_provider_name} identity provider in FusionAuth"
                )
            })?;
        tracing::debug!(%identity_provider_id, "resolved MCP identity provider ID from FusionAuth");

        Ok(Self {
            client: Arc::new(client),
            identity_provider_id,
        })
    }
}

impl OAuthProvider for FusionAuthOAuthProvider {
    #[tracing::instrument(skip(self), err)]
    fn construct_authorize_url(&self, state: &str) -> anyhow::Result<String> {
        self.client.construct_oauth2_authorize_url(
            &self.identity_provider_id,
            None,
            Some(state.to_owned()),
        )
    }

    fn exchange_authorization_code<'a>(&'a self, code: &'a str) -> UpstreamTokensFuture<'a> {
        let span = tracing::debug_span!("FusionAuthOAuthProvider::exchange_authorization_code");
        Box::pin(
            async move {
                let grant = self
                    .client
                    .complete_authorization_code_grant(code)
                    .await
                    .map_err(anyhow::Error::from)?;

                Ok(UpstreamTokens {
                    access_token: grant.access_token.into(),
                    refresh_token: grant.refresh_token.into(),
                    expires_in: grant.expires_in,
                })
            }
            .instrument(span),
        )
    }

    fn refresh_access_token<'a>(
        &'a self,
        refresh_token: &'a RefreshToken,
    ) -> UpstreamTokensFuture<'a> {
        let span = tracing::debug_span!("FusionAuthOAuthProvider::refresh_access_token");
        Box::pin(
            async move {
                let grant = self
                    .client
                    .complete_refresh_token_grant(refresh_token.as_str())
                    .await
                    .map_err(anyhow::Error::from)?;

                Ok(UpstreamTokens {
                    access_token: grant.access_token.into(),
                    refresh_token: grant.refresh_token.into(),
                    expires_in: grant.expires_in,
                })
            }
            .instrument(span),
        )
    }
}
