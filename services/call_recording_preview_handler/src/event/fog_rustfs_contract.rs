//! Exercise the preview handler's source presign and JPEG upload helpers.

use super::{presign_source_object, upload_preview_jpeg};
use aws_sdk_s3::primitives::ByteStream;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires disposable RustFS; use run-remaining-callers.sh"]
async fn call_preview_s3_helpers_use_rustfs() -> anyhow::Result<()> {
    let endpoint = std::env::var("FOG_DIRECT_S3_PROXY_URL")?;
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let s3 = macro_aws_config::s3_client().await;
    let bucket = "macro-call-recording-local";
    s3.create_bucket().bucket(bucket).send().await?;
    s3.put_object()
        .bucket(bucket)
        .key("recordings/synthetic.wav")
        .body(ByteStream::from_static(b"synthetic recording bytes"))
        .send()
        .await?;
    let signed = presign_source_object(
        &s3,
        bucket,
        "recordings/synthetic.wav",
        Duration::from_secs(120),
    )
    .await?;
    assert!(signed.starts_with(&format!("{endpoint}/{bucket}/")));
    let response = reqwest::get(signed).await?;
    assert!(response.status().is_success());
    assert_eq!(
        response.bytes().await?.as_ref(),
        b"synthetic recording bytes"
    );

    let temporary = tempfile::tempdir()?;
    let preview = temporary.path().join("synthetic.jpg");
    tokio::fs::write(&preview, b"synthetic jpeg bytes").await?;
    upload_preview_jpeg(&s3, bucket, "previews/synthetic.jpg", &preview).await?;
    let result = s3
        .get_object()
        .bucket(bucket)
        .key("previews/synthetic.jpg")
        .send()
        .await?;
    assert_eq!(result.content_type(), Some("image/jpeg"));
    assert_eq!(
        result.body.collect().await?.into_bytes().as_ref(),
        b"synthetic jpeg bytes"
    );
    Ok(())
}
