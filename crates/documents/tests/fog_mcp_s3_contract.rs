//! Exercise MCP's document upload URL adapter against disposable RustFS.

use std::time::Duration;

use base64::Engine;
use documents::domain::ports::PresignedUploadUrlPort;
use documents::outbound::s3_upload_url::S3UploadUrlAdapter;
use model::document::ContentType;
use sha2::{Digest, Sha256};

#[tokio::test]
#[ignore = "requires disposable RustFS; use run-direct-clients.sh"]
async fn mcp_upload_adapter_uses_rustfs() -> anyhow::Result<()> {
    let endpoint = std::env::var("FOG_DIRECT_S3_PROXY_URL")?;
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let s3 = macro_aws_config::s3_client().await;
    for bucket in ["doc-storage", "docx-upload"] {
        s3.create_bucket().bucket(bucket).send().await?;
    }
    let adapter = S3UploadUrlAdapter::new(s3.clone(), "doc-storage", "docx-upload");
    let body = b"synthetic MCP document upload";
    let sha = Sha256::digest(body);
    let sha_hex = hex::encode(sha);
    let checksum = base64::engine::general_purpose::STANDARD.encode(sha);
    let browser = reqwest::Client::new();

    for (bucket, key, signed) in [
        (
            "doc-storage",
            "mcp/document space+plus.txt",
            adapter
                .put_document_storage_presigned_url(
                    "mcp/document space+plus.txt",
                    &sha_hex,
                    ContentType::Default,
                )
                .await?,
        ),
        (
            "docx-upload",
            "mcp/docx space+plus.docx",
            adapter
                .put_docx_upload_presigned_url(
                    "mcp/docx space+plus.docx",
                    &sha_hex,
                    ContentType::Default,
                )
                .await?,
        ),
    ] {
        assert!(signed.starts_with(&format!("{endpoint}/{bucket}/")));
        assert!(signed.contains("X-Amz-Signature="));
        let response = browser
            .put(&signed)
            .header("content-type", ContentType::Default.mime_type())
            .header("x-amz-checksum-sha256", &checksum)
            .body(body.to_vec())
            .send()
            .await?;
        assert!(
            response.status().is_success(),
            "signed MCP PUT: {}",
            response.status()
        );
        let downloaded = s3.get_object().bucket(bucket).key(key).send().await?;
        assert_eq!(downloaded.body.collect().await?.into_bytes().as_ref(), body);
    }

    assert!(
        adapter
            .get_document_storage_presigned_url("mcp/document space+plus.txt")
            .await?
            .is_none()
    );
    let read_adapter = S3UploadUrlAdapter::new(s3.clone(), "doc-storage", "docx-upload")
        .with_s3_read_urls(Duration::from_secs(120));
    let read_url = read_adapter
        .get_document_storage_presigned_url("mcp/document space+plus.txt")
        .await?
        .expect("S3 read URL was enabled");
    assert!(read_url.starts_with(&format!("{endpoint}/doc-storage/")));
    assert!(read_url.contains("X-Amz-Signature="));
    let read_response = browser.get(read_url).send().await?;
    assert!(read_response.status().is_success());
    assert_eq!(read_response.bytes().await?.as_ref(), body);

    // Local document copies and snapshots must persist in the same S3 store.
    assert!(adapter.get_snapshot("missing-document").await?.is_none());
    adapter
        .upload_snapshot("missing-document", b"synthetic snapshot".to_vec())
        .await?;
    assert_eq!(
        adapter.get_snapshot("missing-document").await?,
        Some(b"synthetic snapshot".to_vec())
    );
    adapter
        .copy_object("mcp/document space+plus.txt", "mcp/copied.txt")
        .await?;
    assert_eq!(
        s3.get_object()
            .bucket("doc-storage")
            .key("mcp/copied.txt")
            .send()
            .await?
            .body
            .collect()
            .await?
            .into_bytes()
            .as_ref(),
        body
    );
    Ok(())
}
