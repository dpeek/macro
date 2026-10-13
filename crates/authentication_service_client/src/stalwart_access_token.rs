use crate::AuthServiceClient;
use crate::error::{AuthServiceClientError, GenericErrorResponse};
use model::authentication::stalwart_token::StalwartAccessToken;

impl AuthServiceClient {
    /// Exchange the Stalwart refresh grant kept on the FusionAuth identity link.
    pub async fn get_stalwart_access_token(
        &self,
        fusionauth_user_id: &str,
        email: &str,
    ) -> Result<StalwartAccessToken, AuthServiceClientError> {
        let response = self
            .client
            .get(format!("{}/internal/stalwart_access_token", self.url))
            .query(&[("fusionauth_user_id", fusionauth_user_id), ("email", email)])
            .send()
            .await
            .map_err(|error| AuthServiceClientError::RequestBuildError {
                details: error.to_string(),
            })?;
        match response.status() {
            reqwest::StatusCode::OK => response.json().await.map_err(|error| {
                AuthServiceClientError::Generic(GenericErrorResponse {
                    message: error.to_string(),
                })
            }),
            reqwest::StatusCode::UNAUTHORIZED => Err(AuthServiceClientError::Unauthorized),
            reqwest::StatusCode::FORBIDDEN => Err(AuthServiceClientError::Forbidden),
            reqwest::StatusCode::NOT_FOUND => Err(AuthServiceClientError::NotFound),
            status => Err(AuthServiceClientError::InternalServerError {
                details: format!("Stalwart token endpoint returned {status}"),
            }),
        }
    }
}
