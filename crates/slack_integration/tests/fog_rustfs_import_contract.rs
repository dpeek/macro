//! Exercise the real Slack import S3 adapter against disposable RustFS.

use futures::TryStreamExt;
use sha2::{Digest, Sha256};
use slack_integration::domain::{
    models::{ImportLimits, RegisteredUpload, UploadDescriptor, UploadId},
    ports::ImportStorage,
};
use slack_integration::outbound::s3_storage::S3ImportStorage;

#[tokio::test]
#[ignore = "requires disposable RustFS; use run-slack-call.sh"]
async fn slack_import_adapter_uses_rustfs() -> anyhow::Result<()> {
    let endpoint = std::env::var("FOG_DIRECT_S3_PROXY_URL")?;
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let bucket = "slack-import-staging";
    let s3 = macro_aws_config::s3_client().await;
    s3.create_bucket().bucket(bucket).send().await?;
    let storage = S3ImportStorage::new(s3.clone(), bucket.to_owned(), ImportLimits::default())?;
    let body = b"[]";
    let upload = RegisteredUpload {
        key: "slack-import/01980000-0000-7000-8000-000000000001/01980000-0000-7000-8000-000000000002/users.json"
            .parse()?,
        descriptor: UploadDescriptor {
            upload: UploadId::Users,
            sha256: format!("{:x}", Sha256::digest(body)).parse()?,
            byte_length: body.len() as u64,
            record_count: None,
        },
    };
    assert!(storage.verify(&upload).await.is_err());

    let grant = storage.grant(&upload).await?;
    assert!(grant.url.starts_with(&format!("{endpoint}/{bucket}/")));
    assert!(grant.url.contains("X-Amz-Signature="));
    assert_eq!(
        grant
            .required_headers
            .get("if-none-match")
            .map(String::as_str),
        Some("*")
    );
    assert!(grant.required_headers.contains_key("x-amz-checksum-sha256"));
    let browser = reqwest::Client::new();
    let mut request = browser.put(&grant.url).body(body.to_vec());
    for (name, value) in &grant.required_headers {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    assert!(
        response.status().is_success(),
        "signed Slack PUT: {}",
        response.status()
    );

    let verified = storage.verify(&upload).await?;
    let chunks: Vec<_> = storage.read(&verified).await?.try_collect().await?;
    assert_eq!(chunks.concat(), body);

    let mut duplicate = browser.put(&grant.url).body(body.to_vec());
    for (name, value) in &grant.required_headers {
        duplicate = duplicate.header(name, value);
    }
    assert_eq!(
        duplicate.send().await?.status(),
        reqwest::StatusCode::PRECONDITION_FAILED
    );

    s3.put_object()
        .bucket(bucket)
        .key(upload.key.as_str())
        .content_type("application/json")
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b"{}"))
        .send()
        .await?;
    assert!(storage.read(&verified).await.is_err());
    Ok(())
}
