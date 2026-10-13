//! Verifies Fog-signed mailbox change hints independently of Google's webhook.

use anyhow::{Context, ensure};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use macro_env_var::maybe_env_vars;
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use url::Url;

maybe_env_vars! {
    struct FogStalwartPushJwksUrl;
    struct FogStalwartPushIssuer;
    struct FogStalwartPushAudience;
    struct FogStalwartPushSender;
    struct FogStalwartPushTopic;
    struct FogStalwartResolveIp;
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    alg: String,
    #[serde(rename = "use")]
    use_field: String,
    n: String,
    e: String,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    email: String,
    email_verified: bool,
}

/// The local Stalwart push trust configuration and HTTPS client.
pub struct StalwartPushVerifier {
    jwks_url: Url,
    issuer: String,
    audience: String,
    sender: String,
    topic: String,
    http: reqwest::Client,
}

impl StalwartPushVerifier {
    /// Returns `None` until this local deployment enables Fog push.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(jwks_url) = FogStalwartPushJwksUrl::new() else {
            return Ok(None);
        };
        let jwks_url = Url::parse(jwks_url.as_ref()).context("invalid Fog JWKS URL")?;
        ensure!(jwks_url.scheme() == "https", "Fog JWKS URL must use HTTPS");
        let required = |value: Option<&str>, name: &'static str| {
            value
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("{name} is not configured"))
        };
        let issuer = required(
            FogStalwartPushIssuer::new().as_ref().map(AsRef::as_ref),
            "Fog push issuer",
        )?;
        let audience = required(
            FogStalwartPushAudience::new().as_ref().map(AsRef::as_ref),
            "Fog push audience",
        )?;
        let sender = required(
            FogStalwartPushSender::new().as_ref().map(AsRef::as_ref),
            "Fog push sender",
        )?;
        let topic = required(
            FogStalwartPushTopic::new().as_ref().map(AsRef::as_ref),
            "Fog push topic",
        )?;
        let mut http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10));
        if let Some(ip) = FogStalwartResolveIp::new() {
            let ip: IpAddr = ip.as_ref().parse().context("invalid Stalwart proxy IP")?;
            let host = jwks_url.host_str().context("Fog JWKS URL has no host")?;
            http = http.resolve(host, SocketAddr::new(ip, 443));
        }
        Ok(Some(Self {
            jwks_url,
            issuer,
            audience,
            sender,
            topic,
            http: http.build()?,
        }))
    }

    /// Fetches Stalwart's current signing keys. Network errors should be retried.
    pub async fn fetch_keys(&self) -> anyhow::Result<serde_json::Value> {
        let response = self.http.get(self.jwks_url.clone()).send().await?;
        ensure!(
            response.status().is_success(),
            "Fog JWKS returned {}",
            response.status()
        );
        Ok(response.json().await?)
    }

    /// Rejects any token outside the configured key, issuer, audience, sender and topic.
    pub fn verify(
        &self,
        bearer: &str,
        subscription: &str,
        jwks: &serde_json::Value,
    ) -> anyhow::Result<()> {
        ensure!(subscription == self.topic, "Fog push topic did not match");
        let header = decode_header(bearer).context("invalid Fog JWT header")?;
        ensure!(
            header.alg == Algorithm::RS256,
            "unexpected Fog JWT algorithm"
        );
        let kid = header.kid.context("Fog JWT has no key ID")?;
        let jwks: Jwks = serde_json::from_value(jwks.clone()).context("invalid Fog JWKS")?;
        let key = jwks
            .keys
            .iter()
            .find(|key| {
                key.kid == kid && key.kty == "RSA" && key.alg == "RS256" && key.use_field == "sig"
            })
            .context("Fog signing key was not found")?;
        let decoding_key = DecodingKey::from_rsa_components(&key.n, &key.e)?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.audience.as_str()]);
        let claims = decode::<Claims>(bearer, &decoding_key, &validation)
            .context("invalid Fog JWT")?
            .claims;
        ensure!(claims.email_verified, "Fog sender email was not verified");
        ensure!(
            claims.sub == self.sender && claims.email == self.sender,
            "Fog sender did not match"
        );
        Ok(())
    }
}
