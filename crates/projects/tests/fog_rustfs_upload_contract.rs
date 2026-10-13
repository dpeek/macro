//! Exercise project upload destinations against disposable RustFS.

use base64::Engine;
use model::document::{ContentType, FileType};
use projects::domain::ports::ProjectUploadUrlPort;
use projects::outbound::s3_upload_urls::S3ProjectUploadAdapter;
use s3_key::BulkUploadStagingKey;
use sha2::{Digest, Sha256};

#[tokio::test]
#[ignore = "requires disposable RustFS; use run-static-project.sh"]
async fn project_upload_adapter_uses_rustfs() -> anyhow::Result<()> {
    let endpoint = std::env::var("FOG_DIRECT_S3_PROXY_URL")?;
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let s3 = macro_aws_config::s3_client().await;
    for bucket in ["bulk-upload-staging", "doc-storage", "docx-upload"] {
        s3.create_bucket().bucket(bucket).send().await?;
    }
    let adapter = S3ProjectUploadAdapter::new(
        s3.clone(),
        "doc-storage",
        "docx-upload",
        "bulk-upload-staging",
    );
    assert_eq!(adapter.document_storage_bucket(), "doc-storage");
    assert_eq!(adapter.docx_upload_bucket(), "docx-upload");
    let body = b"synthetic project upload";
    let digest = Sha256::digest(body);
    let sha_hex = format!("{digest:x}");
    let checksum = base64::engine::general_purpose::STANDARD.encode(digest);
    let content_type = ContentType::Default;
    let zip_type: ContentType = FileType::Zip.into();
    let uploads = [
        (
            "bulk-upload-staging",
            "extract/synthetic-request",
            zip_type.mime_type(),
            adapter
                .put_upload_zip_staging_presigned_url(
                    BulkUploadStagingKey::new("synthetic-request"),
                    sha_hex.clone(),
                )
                .await?,
        ),
        (
            "doc-storage",
            "projects/document space+plus.txt",
            content_type.mime_type(),
            adapter
                .put_document_storage_presigned_url(
                    "projects/document space+plus.txt".to_owned(),
                    sha_hex.clone(),
                    content_type.clone(),
                )
                .await?,
        ),
        (
            "docx-upload",
            "projects/docx space+plus.docx",
            content_type.mime_type(),
            adapter
                .put_docx_upload_presigned_url(
                    "projects/docx space+plus.docx".to_owned(),
                    sha_hex,
                    content_type.clone(),
                )
                .await?,
        ),
    ];
    let browser = reqwest::Client::new();
    for (bucket, key, mime, signed) in uploads {
        assert!(signed.starts_with(&format!("{endpoint}/{bucket}/")));
        assert!(signed.contains("X-Amz-Signature="));
        let response = browser
            .put(&signed)
            .header("content-type", mime)
            .header("x-amz-checksum-sha256", &checksum)
            .body(body.to_vec())
            .send()
            .await?;
        assert!(
            response.status().is_success(),
            "signed project PUT: {}",
            response.status()
        );
        let head = s3
            .head_object()
            .bucket(bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await?;
        assert_eq!(head.content_type(), Some(mime));
        assert_eq!(head.checksum_sha256(), Some(checksum.as_str()));
        let object = s3.get_object().bucket(bucket).key(key).send().await?;
        assert_eq!(object.body.collect().await?.into_bytes().as_ref(), body);
    }
    Ok(())
}
