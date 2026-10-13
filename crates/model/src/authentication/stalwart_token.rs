#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct StalwartAccessToken {
    pub access_token: String,
    pub expires_in: u64,
    /// Scopes copied from Stalwart's trusted refresh response.
    pub scope: Option<String>,
}
