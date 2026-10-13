//! ZIP staging, extraction, object transfer and bulk status against disposable FogAWS/RustFS.

use super::{
    FileSystemNodeWithIds, S3Destination, build_file_system, extract_zip_file,
    move_files_to_cloud_storage_bucket,
};
use aws_sdk_s3::primitives::ByteStream;
use dynamodb_client::DynamodbClient;
use model::folder::S3DestinationMap;
use models_bulk_upload::{S3ObjectInfo, UploadDocumentStatus, UploadFolderStatus};
use std::{collections::HashMap, fs::File, io::Write};

#[tokio::test]
#[ignore = "requires disposable FogAWS/RustFS; use test-macro-aws-workflows.sh"]
async fn m5_bulk_zip_extracts_into_document_storage() -> Result<(), lambda_runtime::Error> {
    assert!(!cfg!(feature = "local"), "local feature skips staging GET");
    let endpoint = std::env::var("FOG_AWS_TEST_ENDPOINT")?;
    let config = macro_aws_config::local_aws_config(&endpoint).await;
    let state = DynamodbClient::new(&config, Some("bulk-upload".to_owned()));
    let s3 = macro_aws_config::s3_client().await;
    if s3
        .head_bucket()
        .bucket("bulk-upload-staging")
        .send()
        .await
        .is_err()
    {
        s3.create_bucket()
            .bucket("bulk-upload-staging")
            .send()
            .await?;
    }
    let request_id = "m5-bulk-zip";
    let document_id = "01a0914f-1fde-7873-80ba-6eef315d3b54";
    let output_key = format!("macro|m5@example.test/{document_id}/1");
    let created = state
        .bulk_upload
        .create_bulk_upload_request(request_id, "macro|m5@example.test", Some("M5 Folder"), None)
        .await?;
    let temporary = tempfile::tempdir()?;
    let zip_path = temporary.path().join("upload.zip");
    {
        let mut archive = zip::ZipWriter::new(File::create(&zip_path)?);
        archive.start_file(
            "M5 Folder/note.txt",
            zip::write::SimpleFileOptions::default(),
        )?;
        archive.write_all(b"m5 bulk document bytes")?;
        archive.finish()?;
    }
    s3.put_object()
        .bucket("bulk-upload-staging")
        .key(&created.key)
        .body(ByteStream::from_path(&zip_path).await?)
        .send()
        .await?;
    let downloaded = temporary.path().join("downloaded.zip");
    let staged = s3
        .get_object()
        .bucket("bulk-upload-staging")
        .key(&created.key)
        .send()
        .await?;
    let mut body = staged.body.into_async_read();
    tokio::io::copy(&mut body, &mut tokio::fs::File::create(&downloaded).await?).await?;
    let extracted = temporary.path().join("extracted");
    std::fs::create_dir(&extracted)?;
    let root = extract_zip_file(&downloaded, &extracted, Some("M5 Folder"))?;
    assert_eq!(root, "M5 Folder");
    let tree = build_file_system(&extracted).await?;
    assert_eq!(tree.items.len(), 1);
    let item = tree.items.into_iter().next().unwrap();
    let file_system = FileSystemNodeWithIds::Folder {
        project_id: "m5-bulk-root-project".to_owned(),
        content: HashMap::from([(
            item.full_name.clone(),
            FileSystemNodeWithIds::File {
                item,
                document_id: document_id.to_owned(),
            },
        )]),
    };
    let destinations: S3DestinationMap = HashMap::from([(
        document_id.to_owned(),
        S3Destination::Internal(S3ObjectInfo {
            bucket: "doc-storage".to_owned(),
            key: output_key.clone(),
        }),
    )]);
    let status = move_files_to_cloud_storage_bucket(
        &s3,
        &state,
        request_id,
        &extracted,
        &file_system,
        &destinations,
    )
    .await?;
    assert_eq!(status, UploadFolderStatus::Completed);
    state
        .bulk_upload
        .update_bulk_upload_request_status(
            request_id,
            UploadFolderStatus::Completed,
            None,
            Some("m5-bulk-root-project"),
        )
        .await?;
    let statuses = state
        .bulk_upload
        .get_bulk_upload_document_statuses(request_id)
        .await?;
    assert_eq!(statuses.documents.len(), 1);
    assert_eq!(statuses.documents[0].document_id, document_id);
    assert!(matches!(
        statuses.documents[0].status,
        UploadDocumentStatus::Completed
    ));
    assert_eq!(statuses.root_project_id, "m5-bulk-root-project");
    let object = s3
        .get_object()
        .bucket("doc-storage")
        .key(&output_key)
        .send()
        .await?;
    assert_eq!(
        object.body.collect().await?.into_bytes().as_ref(),
        b"m5 bulk document bytes"
    );
    // The bulk object emits a real finalizer notification. Consume this
    // fixture's event so the following document-finalizer test starts with a
    // queue containing only its own upload.
    let sqs = aws_sdk_sqs::Client::new(&config);
    let queue = sqs
        .get_queue_url()
        .queue_name("document-upload-finalizer-queue")
        .send()
        .await?
        .queue_url
        .ok_or_else(|| anyhow::anyhow!("missing document finalizer queue URL"))?;
    let mut notification_received = false;
    for _ in 0..20 {
        let page = sqs
            .receive_message()
            .queue_url(&queue)
            .wait_time_seconds(1)
            .send()
            .await?;
        for message in page.messages() {
            if message
                .body()
                .is_some_and(|body| body.contains(document_id))
            {
                sqs.delete_message()
                    .queue_url(&queue)
                    .receipt_handle(
                        message
                            .receipt_handle()
                            .ok_or_else(|| anyhow::anyhow!("missing receipt"))?,
                    )
                    .send()
                    .await?;
                notification_received = true;
                break;
            }
        }
        if notification_received {
            break;
        }
    }
    assert!(
        notification_received,
        "bulk document notification was not delivered"
    );
    s3.delete_object()
        .bucket("doc-storage")
        .key(&output_key)
        .send()
        .await?;
    s3.delete_object()
        .bucket("bulk-upload-staging")
        .key(&created.key)
        .send()
        .await?;
    Ok(())
}
