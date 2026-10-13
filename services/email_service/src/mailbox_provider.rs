//! Mailbox HTTP client shared by the service and workers.

use anyhow::{Context, ensure};
use email_api_client::GmailApiClientRepository;
use gmail_client::GmailClient;
use macro_env_var::optional_read_env_var;
use url::Url;

/// Uses Stalwart when configured, otherwise Macro's Google client.
pub fn repository(subscription_topic: String) -> anyhow::Result<GmailApiClientRepository> {
    Ok(GmailApiClientRepository::new(client(subscription_topic)?))
}

/// Builds the raw client too, so auxiliary API paths use the same provider.
pub fn client(subscription_topic: String) -> anyhow::Result<GmailClient> {
    let Some(url) = optional_read_env_var("FOG_STALWART_GMAIL_URL")? else {
        return Ok(GmailClient::new(subscription_topic));
    };
    let parsed = Url::parse(&url).context("invalid Stalwart Gmail-compatible URL")?;
    ensure!(
        parsed.scheme() == "https",
        "Stalwart mailbox URL must use HTTPS"
    );
    let origin = parsed.origin().ascii_serialization();
    Ok(GmailClient::new_with_urls(
        subscription_topic,
        url,
        format!("{origin}/people/v1"),
        format!("{origin}/gmail/v1/jwks"),
        "macro-stalwart-webhook".to_string(),
    ))
}
