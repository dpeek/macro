#![deny(missing_docs)]

//! This crate creates a standard way to make AWS configs.

pub use aws_config::SdkConfig;
use macro_env_var::{env_var, maybe_env_var};

maybe_env_var! {
    #[derive(Clone)]
    pub struct LocalAwsUrl;
}

maybe_env_var! {
    /// Browser-facing LocalStack endpoint, including any reverse-proxy path prefix.
    #[derive(Clone)]
    pub struct LocalAwsPublicUrl;
}

maybe_env_var! {
    /// S3-compatible endpoint used independently of the local SQS/DynamoDB endpoint.
    #[derive(Clone)]
    pub struct AwsEndpointUrlS3;
}

env_var! {
    /// Access key required when using a separate S3-compatible endpoint.
    pub struct AwsAccessKeyId;
}

env_var! {
    /// Secret key required when using a separate S3-compatible endpoint.
    pub struct AwsSecretAccessKey;
}

/// Creates an S3 client
#[cfg(feature = "s3")]
pub async fn s3_client() -> aws_sdk_s3::Client {
    let separate_endpoint = AwsEndpointUrlS3::new().is_some();
    let base = if separate_endpoint {
        // Keep explicit credentials mandatory so the SDK cannot fall back to
        // a developer's shared AWS profile or instance identity for RustFS.
        let _ = (
            AwsAccessKeyId::unwrap_new(),
            AwsSecretAccessKey::unwrap_new(),
        );
        // The common local config sets an explicit endpoint and dummy credentials
        // for SQS/DynamoDB/KMS. Let the SDK resolve its standard S3 endpoint and
        // AWS credentials from the environment instead.
        aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region("us-east-1")
            .load()
            .await
    } else {
        get_macro_aws_config().await
    };
    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&base)
            .force_path_style(separate_endpoint || is_local_aws())
            .build(),
    )
}

/// Creates an SQS client
#[cfg(feature = "sqs")]
pub async fn sqs_client() -> aws_sdk_sqs::Client {
    aws_sdk_sqs::Client::new(&get_macro_aws_config().await)
}

/// Creates a aws_config to use.
/// If you provide `LOCAL_AWS_URL` environment variable we create a local aws
/// config with test credentials.
/// Otherwise we load normally.
pub async fn get_macro_aws_config() -> aws_config::SdkConfig {
    if let Some(local_aws_url) = LocalAwsUrl::new() {
        local_aws_config(local_aws_url.as_ref()).await
    } else {
        aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region("us-east-1")
            .load()
            .await
    }
}

/// Creates an AWS SDK config pointed at a LocalStack endpoint.
pub async fn local_aws_config(local_aws_url: &str) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region("us-east-1")
        .test_credentials()
        .endpoint_url(local_aws_url)
        .load()
        .await
}

/// Returns if the aws config is local or not
pub fn is_local_aws() -> bool {
    LocalAwsUrl::new().is_some()
}

/// Returns whether S3 has an endpoint and credentials separate from the default AWS config.
pub fn has_s3_endpoint_override() -> bool {
    AwsEndpointUrlS3::new().is_some()
}

/// internal method to transform the local aws url
fn transform_local_url(url: &str, public_url: Option<&str>) -> String {
    // NOTE: it is ok to use expect as this is only run locally
    let parsed = url::Url::parse(url).expect("valid url");
    let host = parsed.host_str().unwrap();
    let port = parsed.port().unwrap_or(4566);
    let path = parsed.path();
    let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();

    let origin = public_url
        .map(|url| url.trim_end_matches('/').to_owned())
        .unwrap_or_else(|| format!("http://localhost:{port}"));

    // Path-style LocalStack URLs generated inside Docker use `localstack` as
    // the host, which the browser on the host machine cannot resolve. Keep the
    // existing path (`/{bucket}/{key}`) and use the browser-facing origin.
    if host == "localstack" || host == "localhost" {
        return format!("{origin}{path}{query}");
    }

    // hostname should be in the form {asset}.localstack or {asset}.localhost
    let asset = host
        .strip_suffix(".localstack")
        .or_else(|| host.strip_suffix(".localhost"))
        .unwrap();

    format!("{origin}/{asset}{path}{query}")
}

/// Transforms a localstack url into one that will work within the app
/// For example, presigned urls for localstack come out as `http://{BUCKET_NAME}.localstack:{PORT}`
/// but we need them to be formulated as `http://localhost:{PORT}/bucket-name`.
/// `LOCAL_AWS_PUBLIC_URL`, when set by the local stack, supplies the browser-facing
/// endpoint so named instances can use their HTTPS storage proxy.
pub fn transform_aws_url(url: &str) -> String {
    if AwsEndpointUrlS3::new().is_some() {
        return url.to_string();
    }
    if is_local_aws() {
        let public_url = LocalAwsPublicUrl::new();
        return transform_local_url(url, public_url.as_ref().map(|url| url.as_ref()));
    }
    url.to_string()
}

/// internal method to transform a browser-facing local url into one reachable
/// from inside the docker network
fn transform_internal_url(url: &str, local_aws_url: &str, public_url: Option<&str>) -> String {
    // NOTE: it is ok to use expect as this is only run locally
    let parsed = url::Url::parse(url).expect("valid url");
    let host = parsed.host_str().unwrap();
    let path = parsed.path();
    let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();

    // The HTTPS app proxy exposes storage below a path prefix. Match only
    // that configured endpoint and remove its prefix before calling S3.
    if let Some(public) = public_url.and_then(|url| url::Url::parse(url).ok())
        && parsed.origin() == public.origin()
        && let Some(key) = path.strip_prefix(&format!("{}/", public.path().trim_end_matches('/')))
    {
        return format!("{}/{key}{query}", local_aws_url.trim_end_matches('/'));
    }

    // Browser-facing local URLs use `localhost`, which inside a container
    // resolves to the container itself. Use the SDK endpoint, including its
    // internal port, so service-to-service fetches reach LocalStack. Leave any other
    // host untouched.
    if host == "localhost" || host == "localstack" {
        return format!("{}{path}{query}", local_aws_url.trim_end_matches('/'));
    }

    url.to_string()
}

/// Transforms a browser-facing local URL into one reachable from inside the
/// app's own containers when fetching an object server-side.
///
/// The inverse of [`transform_aws_url`]: remove the configured public endpoint's
/// origin and proxy prefix so container clients reach LocalStack directly.
/// Legacy localhost URLs are also supported. No-op outside local AWS.
pub fn transform_aws_url_for_internal_fetch(url: &str) -> String {
    if AwsEndpointUrlS3::new().is_some() {
        return url.to_string();
    }
    if let Some(local_aws_url) = LocalAwsUrl::new() {
        let public_url = LocalAwsPublicUrl::new();
        return transform_internal_url(
            url,
            local_aws_url.as_ref(),
            public_url.as_ref().map(|url| url.as_ref()),
        );
    }
    url.to_string()
}

#[cfg(test)]
mod test;
