//! Exercise the shared S3 wrapper used by email attachments and conversion.

use base64::Engine;
use s3_client::S3;
use sha2::{Digest, Sha256};

#[tokio::test]
#[ignore = "requires disposable RustFS; use run-document-email.sh"]
async fn shared_email_s3_adapter_uses_rustfs() -> anyhow::Result<()> {
    let endpoint = std::env::var("FOG_DIRECT_S3_PROXY_URL")?;
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let bucket = "macro-email-attachments";
    let s3 = macro_aws_config::s3_client().await;
    s3.create_bucket().bucket(bucket).send().await?;
    let storage = S3::new(s3.clone());
    let original = "attachments/synthetic-original";
    let uploaded = "attachments/synthetic-browser";
    let copied = "attachments/synthetic-copy";
    let body = b"synthetic email attachment";

    assert!(!storage.exists(bucket, original).await?);
    storage.put(bucket, original, body).await?;
    assert!(storage.exists(bucket, original).await?);
    assert_eq!(storage.get(bucket, original).await?, body);

    let signed_read = storage.get_presigned_url(bucket, original, 120).await?;
    assert!(
        signed_read
            .uri()
            .starts_with(&format!("{endpoint}/{bucket}/"))
    );
    let browser = reqwest::Client::new();
    let read_response = browser.get(signed_read.uri()).send().await?;
    assert!(read_response.status().is_success());
    assert_eq!(read_response.bytes().await?.as_ref(), body);

    let digest = Sha256::digest(body);
    let sha_hex = format!("{digest:x}");
    let checksum = base64::engine::general_purpose::STANDARD.encode(digest);
    let signed_put = storage
        .put_presigned_url(bucket, uploaded, &sha_hex, "application/octet-stream")
        .await?;
    assert!(signed_put.starts_with(&format!("{endpoint}/{bucket}/")));
    let write_response = browser
        .put(&signed_put)
        .header("content-type", "application/octet-stream")
        .header("x-amz-checksum-sha256", checksum)
        .body(body.to_vec())
        .send()
        .await?;
    assert!(write_response.status().is_success());
    assert_eq!(storage.get(bucket, uploaded).await?, body);

    storage.inner_bucket_copy(bucket, original, copied).await?;
    assert_eq!(storage.get(bucket, copied).await?, body);
    storage.delete(bucket, original).await?;
    storage.delete(bucket, original).await?;
    storage.delete_folder(bucket, "attachments/").await?;
    for key in [original, uploaded, copied] {
        assert!(!storage.exists(bucket, key).await?);
    }
    Ok(())
}
