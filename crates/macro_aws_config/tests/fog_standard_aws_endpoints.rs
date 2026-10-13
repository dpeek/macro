//! Isolated proof for the SDK's standard per-service endpoint settings.
//! Run through config/macro/local-aws-contract/run-dual-endpoint.sh.

use aws_sdk_s3::{presigning::PresigningConfig, primitives::ByteStream};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires disposable RustFS and LocalStack; use run-dual-endpoint.sh"]
async fn standard_endpoints_route_s3_and_sqs_separately() -> Result<(), Box<dyn std::error::Error>>
{
    let s3_endpoint = std::env::var("FOG_DUAL_S3_PROXY_URL")?;
    let sqs_endpoint = std::env::var("FOG_DUAL_SQS_PROXY_URL")?;
    assert_ne!(s3_endpoint, sqs_endpoint);
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, s3_endpoint);
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_SQS")?, sqs_endpoint);
    assert_eq!(std::env::var("LOCAL_AWS_URL")?, sqs_endpoint);

    // Macro's S3 client now uses the SDK's native endpoint and credential
    // resolution, with path-style addressing for the browser route. SQS keeps
    // the existing LOCAL_AWS_URL path and dummy local credentials.
    let s3 = macro_aws_config::s3_client().await;
    let sqs = macro_aws_config::sqs_client().await;

    let bucket = "fog-contract-standard-endpoint";
    s3.create_bucket().bucket(bucket).send().await?;
    s3.head_bucket().bucket(bucket).send().await?;
    let signed = s3
        .put_object()
        .bucket(bucket)
        .key("direct/signed.txt")
        .content_type("text/plain")
        .presigned(PresigningConfig::expires_in(Duration::from_secs(120))?)
        .await?;
    assert!(
        signed
            .uri()
            .starts_with(&format!("{s3_endpoint}/{bucket}/"))
    );
    assert!(
        signed
            .uri()
            .contains("X-Amz-Credential=fogcontractaccess%2F")
    );
    let response = reqwest::Client::new()
        .put(signed.uri())
        .header("content-type", "text/plain")
        .body("synthetic direct endpoint payload")
        .send()
        .await?;
    assert!(
        response.status().is_success(),
        "standard SDK signed PUT: {}",
        response.status()
    );
    let body = s3
        .get_object()
        .bucket(bucket)
        .key("direct/signed.txt")
        .send()
        .await?
        .body
        .collect()
        .await?
        .into_bytes();
    assert_eq!(body.as_ref(), b"synthetic direct endpoint payload");
    s3.put_object()
        .bucket(bucket)
        .key("direct/sdk.txt")
        .body(ByteStream::from_static(b"synthetic SDK payload"))
        .send()
        .await?;

    let queue = sqs
        .create_queue()
        .queue_name("fog-contract-standard-endpoint")
        .send()
        .await?;
    assert!(queue.queue_url().is_some());

    // Macro's current local config installs an explicit common endpoint and
    // dummy credentials. Record which endpoint wins before removing its S3
    // client override; a presigned URL needs no backend request for this.
    let legacy = macro_aws_config::local_aws_config(&sqs_endpoint).await;
    let legacy_s3 = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&legacy)
            .force_path_style(true)
            .build(),
    );
    let legacy_signed = legacy_s3
        .put_object()
        .bucket(bucket)
        .key("legacy/signed.txt")
        .presigned(PresigningConfig::expires_in(Duration::from_secs(120))?)
        .await?;
    let legacy_url = url::Url::parse(legacy_signed.uri())?;
    assert_eq!(legacy_url.origin().ascii_serialization(), sqs_endpoint);
    assert!(
        legacy_signed.uri().contains("X-Amz-Credential=ANOTREAL%2F")
            || legacy_signed.uri().contains("X-Amz-Credential=test%2F")
    );
    Ok(())
}
