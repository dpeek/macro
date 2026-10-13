//! Exercise the real call recording S3 adapter against disposable RustFS.

use aws_sdk_s3::primitives::ByteStream;
use call::domain::ports::RecordingStorage;
use call::outbound::s3_recording_storage::{RecordingCloudFrontConfig, S3RecordingStorage};

#[tokio::test]
#[ignore = "requires disposable RustFS; use run-slack-call.sh"]
async fn call_recording_adapter_uses_rustfs() -> anyhow::Result<()> {
    let endpoint = std::env::var("FOG_DIRECT_S3_PROXY_URL")?;
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let bucket = "macro-call-recording-local";
    let s3 = macro_aws_config::s3_client().await;
    s3.create_bucket().bucket(bucket).send().await?;
    let recording_key = "synthetic-room/recording space+plus.mp4";
    let preview_key = "calls/synthetic-room/recording space+plus/PREVIEW.jpg";
    for (key, bytes, content_type) in [
        (
            format!("calls/{recording_key}"),
            b"synthetic recording".as_slice(),
            "video/mp4",
        ),
        (
            preview_key.to_owned(),
            b"synthetic preview".as_slice(),
            "image/jpeg",
        ),
    ] {
        s3.put_object()
            .bucket(bucket)
            .key(key)
            .content_type(content_type)
            .body(ByteStream::from(bytes.to_vec()))
            .send()
            .await?;
    }
    let storage = S3RecordingStorage::new(
        bucket.to_owned(),
        RecordingCloudFrontConfig {
            distribution_url: "https://unused.example.test".to_owned(),
            signer_public_key_id: String::new(),
            signer_private_key: String::new(),
            presigned_url_expiry_seconds: 120,
        },
    )
    .await;
    let browser = reqwest::Client::new();
    for (url, bytes) in [
        (
            storage.presign_recording_url(recording_key).await?,
            b"synthetic recording".as_slice(),
        ),
        (
            storage.presign_recording_preview_url(preview_key).await?,
            b"synthetic preview".as_slice(),
        ),
    ] {
        assert!(url.starts_with(&format!("{endpoint}/{bucket}/")));
        assert!(url.contains("X-Amz-Signature="));
        let response = browser.get(url).send().await?;
        assert!(response.status().is_success());
        assert_eq!(response.bytes().await?.as_ref(), bytes);
    }

    storage.delete_recording(recording_key).await?;
    storage.delete_recording(recording_key).await?;
    storage.delete_recording_preview(preview_key).await?;
    storage.delete_recording_preview(preview_key).await?;
    for key in [format!("calls/{recording_key}"), preview_key.to_owned()] {
        assert!(
            s3.head_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .is_err()
        );
    }
    Ok(())
}
