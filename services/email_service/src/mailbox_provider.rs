//! Mailbox HTTP client shared by the service and workers.

use anyhow::{Context, ensure};
use email_api_client::GmailApiClientRepository;
use gmail_client::GmailClient;
use macro_env_var::optional_read_env_var;
use url::Url;

/// Configures both wire clients; each token chooses its linked provider.
pub fn repository(subscription_topic: String) -> anyhow::Result<GmailApiClientRepository> {
    let google = GmailApiClientRepository::new(client(subscription_topic.clone()));
    Ok(match stalwart_client(subscription_topic)? {
        Some(stalwart) => google.with_stalwart_client(stalwart),
        None => google,
    })
}

/// Google's raw client is used for verifying Google push notifications.
pub fn client(subscription_topic: String) -> GmailClient {
    GmailClient::new(subscription_topic)
}

fn stalwart_client(subscription_topic: String) -> anyhow::Result<Option<GmailClient>> {
    let Some(url) = optional_read_env_var("FOG_STALWART_GMAIL_URL")? else {
        return Ok(None);
    };
    let parsed = Url::parse(&url).context("invalid Stalwart Gmail-compatible URL")?;
    ensure!(
        parsed.scheme() == "https",
        "Stalwart mailbox URL must use HTTPS"
    );
    let origin = parsed.origin().ascii_serialization();
    Ok(Some(GmailClient::new_with_urls(
        subscription_topic,
        url,
        format!("{origin}/people/v1"),
        format!("{origin}/gmail/v1/jwks"),
        "macro-stalwart-webhook".to_string(),
    )))
}
