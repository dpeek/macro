use super::AccessToken;
use models_email::email::service::link::UserProvider;

#[test]
fn access_token_debug_output_is_redacted() {
    let output = format!("{:?}", AccessToken::new("secret-token-value"));

    assert_eq!(output, "AccessToken([REDACTED])");
    assert!(!output.contains("secret-token-value"));
}

#[test]
fn access_token_carries_its_provider_without_exposing_the_secret() {
    let token = AccessToken::for_provider("secret", UserProvider::Stalwart);
    assert_eq!(token.provider(), UserProvider::Stalwart);
    assert_eq!(token.expose_secret(), "secret");
    assert_eq!(format!("{token:?}"), "AccessToken([REDACTED])");
}
