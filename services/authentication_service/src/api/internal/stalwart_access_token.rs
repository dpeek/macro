use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use fusionauth::{FusionAuthClient, error::FusionAuthClientError};
use macro_authorization::{InternalOnly, MacroAuthorizationExtractor};
use macro_env_var::optional_read_env_var;
use model::authentication::stalwart_token::StalwartAccessToken;
use serde::Deserialize;
use std::sync::Arc;

use crate::api::{
    context::AuthorizationService, oauth2::account_link::replace_identity_provider_grant,
};

#[derive(Deserialize)]
pub struct Params {
    fusionauth_user_id: String,
    email: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct UserInfo {
    email: String,
    email_verified: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailProfile {
    email_address: String,
}

/// Refreshes the sole Stalwart grant held on the FusionAuth identity link.
/// Only internal services may call this endpoint; it never returns a refresh token.
pub async fn handler(
    State(auth_client): State<Arc<FusionAuthClient>>,
    _authorization: MacroAuthorizationExtractor<AuthorizationService, InternalOnly>,
    Query(params): Query<Params>,
) -> Result<Json<StalwartAccessToken>, StatusCode> {
    verified_access_token(&auth_client, &params.fusionauth_user_id, &params.email)
        .await
        .map(Json)
}

/// A Stalwart link is eligible only when its refresh grant resolves to the
/// same verified identity and mailbox as the FusionAuth account.
pub(in crate::api) async fn verified_access_token(
    auth_client: &FusionAuthClient,
    fusionauth_user_id: &str,
    email: &str,
) -> Result<StalwartAccessToken, StatusCode> {
    let issuer = optional_read_env_var("FOG_STALWART_OAUTH_ISSUER")
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let client_id = optional_read_env_var("FOG_STALWART_SSO_CLIENT_ID")
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let idp_id = auth_client
        .get_identity_provider_id_by_name("stalwart")
        .await
        .map_err(|error| match error {
            FusionAuthClientError::NoIdentityProviderFound => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_GATEWAY,
        })?;
    let links = auth_client
        .get_links(fusionauth_user_id, Some(idp_id.clone()))
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let link = links
        .into_iter()
        .find(|link| link.display_name.eq_ignore_ascii_case(email))
        .ok_or(StatusCode::NOT_FOUND)?;
    if link.token.is_empty() {
        return Err(StatusCode::FORBIDDEN);
    }

    let http = reqwest::Client::new();
    let issuer = issuer.trim_end_matches('/');
    let response = http
        .post(format!("{issuer}/auth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id.as_str()),
            ("refresh_token", link.token.as_str()),
        ])
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if matches!(
        response.status(),
        StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED
    ) {
        return Err(StatusCode::FORBIDDEN);
    }
    let token: TokenResponse = response
        .error_for_status()
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .json()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let identity: UserInfo = http
        .get(format!("{issuer}/auth/userinfo"))
        .bearer_auth(&token.access_token)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .error_for_status()
        .map_err(|_| StatusCode::FORBIDDEN)?
        .json()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !identity.email_verified || !identity.email.eq_ignore_ascii_case(email) {
        return Err(StatusCode::FORBIDDEN);
    }
    let mailbox: MailProfile = http
        .get(format!("{issuer}/gmail/v1/users/me/profile"))
        .bearer_auth(&token.access_token)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .error_for_status()
        .map_err(|_| StatusCode::FORBIDDEN)?
        .json()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !mailbox.email_address.eq_ignore_ascii_case(email) {
        return Err(StatusCode::FORBIDDEN);
    }

    if let Some(fresh_token) = token.refresh_token.filter(|value| !value.is_empty()) {
        replace_identity_provider_grant(
            &auth_client,
            &idp_id,
            fusionauth_user_id,
            &link.display_name,
            &fresh_token,
        )
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    }
    Ok(StalwartAccessToken {
        access_token: token.access_token,
        expires_in: token.expires_in,
        scope: token.scope,
    })
}
