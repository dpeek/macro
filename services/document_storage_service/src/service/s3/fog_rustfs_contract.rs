//! Exercise the real document storage S3 adapter against disposable RustFS.

use super::S3Client;
use model_owner::Owner;
use s3_key::{build_cloud_storage_bucket_document_key, build_cloud_storage_bucket_document_prefix};

#[tokio::test]
#[ignore = "requires disposable RustFS; use run-document-email.sh"]
async fn document_storage_adapter_uses_rustfs() -> anyhow::Result<()> {
    let endpoint = std::env::var("FOG_DIRECT_S3_PROXY_URL")?;
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let bucket = "doc-storage";
    let s3 = macro_aws_config::s3_client().await;
    s3.create_bucket().bucket(bucket).send().await?;
    let storage = S3Client::new(s3.clone(), bucket, "docx-upload", "bulk-upload-staging");
    let owner = Owner::from_principal_str("macro|synthetic@example.test")?;
    let document_id = "synthetic-document";
    let source = build_cloud_storage_bucket_document_key(&owner, document_id, 1);
    let destination = build_cloud_storage_bucket_document_key(&owner, document_id, 2);
    let prefix = build_cloud_storage_bucket_document_prefix(&owner, document_id);
    let body = b"synthetic document bytes";

    assert!(!storage.exists(&source).await?);
    storage.upload_document(&source, body.to_vec()).await?;
    assert!(storage.exists(&source).await?);
    assert_eq!(storage.get_document(&source).await?, body);
    storage.copy_document(&source, &destination).await?;
    assert_eq!(storage.get_document(&destination).await?, body);
    let mut names = storage.get_folder_content_names(&prefix).await?;
    names.sort();
    assert_eq!(
        names,
        vec![
            ("1".to_owned(), String::new()),
            ("2".to_owned(), String::new())
        ]
    );

    let browser = reqwest::Client::new();
    let signed_read = storage
        .get_document_storage_presigned_url(&destination, std::time::Duration::from_secs(900))
        .await?;
    assert!(signed_read.starts_with(&format!("{endpoint}/{bucket}/")));
    let read_response = browser.get(signed_read).send().await?;
    assert!(read_response.status().is_success());
    assert_eq!(read_response.bytes().await?.as_ref(), body);

    let snapshot = "sync_service_snapshot/synthetic-document";
    let signed_put = storage.put_snapshot_presigned_url(snapshot).await?;
    assert!(signed_put.starts_with(&format!("{endpoint}/{bucket}/")));
    let put_response = browser
        .put(signed_put)
        .body(b"synthetic snapshot".to_vec())
        .send()
        .await?;
    assert!(put_response.status().is_success());
    let signed_snapshot_read = storage.get_snapshot_presigned_url(snapshot).await?;
    let snapshot_response = browser.get(signed_snapshot_read).send().await?;
    assert!(snapshot_response.status().is_success());
    assert_eq!(
        snapshot_response.bytes().await?.as_ref(),
        b"synthetic snapshot"
    );

    storage.delete_document(&owner, document_id).await?;
    assert!(!storage.exists(&source).await?);
    assert!(!storage.exists(&destination).await?);
    assert!(storage.exists(snapshot).await?);
    Ok(())
}
