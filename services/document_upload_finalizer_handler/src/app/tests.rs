use std::str::FromStr;
use std::sync::Mutex;

use documents::domain::content::{DocumentContent, DocumentContentLocation, DocumentContentState};
use documents::domain::legacy_office_upgrade::{
    LegacyOfficeUpgradeRequest, UpgradedObjectDestination,
};
use documents::domain::models::DocumentError;
use model::document::DocumentBasic;
use model_owner::Owner;

use super::*;

const OWNER: &str = "macro|owner@macro.com";
const DOCUMENT_ID: &str = "01a0914f-1fde-7873-80ba-6eef315d3b50";
const BUCKET: &str = "document-storage";

#[derive(Clone, Debug, PartialEq)]
enum Call {
    SetContent(DocumentContentState, Option<DocumentContentLocation>),
    RequestUpgrade(LegacyOfficeUpgradeRequest),
    Promote(FileType, UpgradedObjectDestination),
    Swap(FileType, FileType),
    CreateInstance,
    CreateBom,
}

#[derive(Default)]
struct Log(Mutex<Vec<Call>>);

impl Log {
    fn push(&self, call: Call) {
        self.0.lock().unwrap().push(call);
    }
    fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().clone()
    }
}

struct FakeDocuments<'a> {
    log: &'a Log,
    file_type: Mutex<Option<String>>,
    content: DocumentContent,
    missing: bool,
}

impl FakeDocuments<'_> {
    fn document(&self) -> DocumentBasic {
        DocumentBasic {
            document_id: DOCUMENT_ID.to_string(),
            document_name: "Upload".to_string(),
            owner: Owner::from_principal_str(OWNER).unwrap(),
            file_type: self.file_type.lock().unwrap().clone(),
            sub_type: None,
            branched_from_id: None,
            branched_from_version_id: None,
            document_family_id: None,
            project_id: None,
            deleted_at: None,
        }
    }
}

impl DocumentUploadMetadataPort for FakeDocuments<'_> {
    async fn get_basic_document(
        &self,
        document_id: &str,
    ) -> Result<Option<DocumentBasic>, DocumentError> {
        assert_eq!(document_id, DOCUMENT_ID);
        Ok((!self.missing).then(|| self.document()))
    }
}

impl UploadFinalizeDocumentPort for FakeDocuments<'_> {
    async fn get_document_content(
        &self,
        _document_context: &DocumentBasic,
    ) -> Result<DocumentContent, DocumentError> {
        Ok(self.content.clone())
    }

    async fn mark_document_uploaded(&self, _document_id: &str) -> Result<(), DocumentError> {
        Ok(())
    }

    async fn set_document_content(
        &self,
        _document_id: &str,
        content: DocumentContent,
    ) -> Result<(), DocumentError> {
        self.log
            .push(Call::SetContent(content.state, content.location));
        Ok(())
    }
}

impl LegacyOfficeUpgradeRepoPort for FakeDocuments<'_> {
    async fn create_document_instance(
        &self,
        _document_id: &str,
        _sha: &str,
    ) -> Result<i64, DocumentError> {
        self.log.push(Call::CreateInstance);
        Ok(99)
    }

    async fn delete_document_instance(&self, _version_id: i64) -> Result<(), DocumentError> {
        Ok(())
    }

    async fn create_document_bom(&self, _document_id: &str) -> Result<i64, DocumentError> {
        self.log.push(Call::CreateBom);
        Ok(5)
    }

    async fn delete_document_bom(&self, _bom_id: i64) -> Result<(), DocumentError> {
        Ok(())
    }

    async fn swap_document_file_type(
        &self,
        _document_id: &str,
        from: FileType,
        to: FileType,
    ) -> Result<bool, DocumentError> {
        self.log.push(Call::Swap(from, to));
        let mut stored = self.file_type.lock().unwrap();
        if stored.as_deref() != Some(from.as_str()) {
            return Ok(false);
        }
        *stored = Some(to.as_str().to_string());
        Ok(true)
    }

    async fn set_document_content(
        &self,
        _document_id: &str,
        content: DocumentContent,
    ) -> Result<(), DocumentError> {
        self.log
            .push(Call::SetContent(content.state, content.location));
        Ok(())
    }
}

struct FakeObjects(bool);

impl DocumentObjectReader for FakeObjects {
    async fn object_exists(&self, _bucket: &str, _key: &str) -> Result<bool, anyhow::Error> {
        Ok(self.0)
    }

    async fn read_utf8_object(&self, _bucket: &str, _key: &str) -> Result<String, anyhow::Error> {
        Ok("# markdown".to_string())
    }
}

struct FakeUpgradeStorage<'a> {
    log: &'a Log,
}

impl LegacyOfficeUpgradeStoragePort for FakeUpgradeStorage<'_> {
    async fn request_upgrade(
        &self,
        request: &LegacyOfficeUpgradeRequest,
    ) -> Result<(), DocumentError> {
        self.log.push(Call::RequestUpgrade(request.clone()));
        Ok(())
    }

    async fn upgraded_object_sha(
        &self,
        _owner: &Owner,
        _document_id: &str,
        _target: FileType,
    ) -> Result<String, DocumentError> {
        Ok("sha".to_string())
    }

    async fn promote_upgraded_object(
        &self,
        _owner: &Owner,
        _document_id: &str,
        target: FileType,
        destination: UpgradedObjectDestination,
    ) -> Result<(), DocumentError> {
        self.log.push(Call::Promote(target, destination));
        Ok(())
    }
}

struct NoMarkdown;

impl MarkdownInitializationPort for NoMarkdown {
    async fn initialize_existing_markdown(
        &self,
        _document_id: &str,
        _markdown: &str,
    ) -> Result<Vec<u8>, DocumentError> {
        Ok(Vec::new())
    }
}

async fn handle(file_type: Option<&str>, content: DocumentContent, key: String) -> Vec<Call> {
    let log = Log::default();
    let finalizer = DocumentUploadFinalizer::new(
        FakeDocuments {
            log: &log,
            file_type: Mutex::new(file_type.map(str::to_string)),
            content,
            missing: false,
        },
        FakeObjects(true),
        FakeUpgradeStorage { log: &log },
    );
    finalizer
        .handle_object_created(
            ObjectCreated {
                bucket: BUCKET.to_string(),
                key,
            },
            &NoMarkdown,
        )
        .await
        .unwrap();
    log.calls()
}

#[tokio::test]
async fn object_created_before_metadata_commit_is_retried() {
    let log = Log::default();
    let finalizer = DocumentUploadFinalizer::new(
        FakeDocuments {
            log: &log,
            file_type: Mutex::new(Some("pdf".to_string())),
            content: DocumentContent::pending(),
            missing: true,
        },
        FakeObjects(true),
        FakeUpgradeStorage { log: &log },
    );
    let error = finalizer
        .handle_object_created(
            ObjectCreated {
                bucket: BUCKET.to_string(),
                key: versioned_key(12),
            },
            &NoMarkdown,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("retrying"));
    assert!(log.calls().is_empty());
}

#[tokio::test]
async fn late_event_after_document_and_object_removal_is_ignored() {
    let log = Log::default();
    let finalizer = DocumentUploadFinalizer::new(
        FakeDocuments {
            log: &log,
            file_type: Mutex::new(Some("pdf".to_string())),
            content: DocumentContent::pending(),
            missing: true,
        },
        FakeObjects(false),
        FakeUpgradeStorage { log: &log },
    );
    finalizer
        .handle_object_created(
            ObjectCreated {
                bucket: BUCKET.to_string(),
                key: versioned_key(12),
            },
            &NoMarkdown,
        )
        .await
        .unwrap();
    assert!(log.calls().is_empty());
}

fn versioned_key(version_id: i64) -> String {
    format!("{OWNER}/{DOCUMENT_ID}/{version_id}")
}

fn upgraded_key(extension: &str) -> String {
    format!("{OWNER}/{DOCUMENT_ID}/upgraded.{extension}")
}

#[tokio::test]
async fn legacy_upload_is_finalized_then_upgrade_requested() {
    for (legacy, target) in [
        ("ppt", FileType::Pptx),
        ("doc", FileType::Docx),
        ("xls", FileType::Xlsx),
    ] {
        let calls = handle(Some(legacy), DocumentContent::pending(), versioned_key(12)).await;

        assert_eq!(
            calls,
            vec![
                Call::SetContent(
                    DocumentContentState::Ready,
                    Some(DocumentContentLocation::ObjectStorage)
                ),
                Call::RequestUpgrade(LegacyOfficeUpgradeRequest {
                    owner: Owner::from_principal_str(OWNER).unwrap(),
                    document_id: DOCUMENT_ID.to_string(),
                    source_version_id: 12,
                    from: FileType::from_str(legacy).unwrap(),
                    to: target,
                }),
            ],
            "{legacy}"
        );
    }
}

#[tokio::test]
async fn modern_uploads_request_no_upgrade() {
    for file_type in ["pptx", "xlsx", "xlsm", "pdf"] {
        let calls = handle(
            Some(file_type),
            DocumentContent::pending(),
            versioned_key(3),
        )
        .await;

        assert!(
            !calls
                .iter()
                .any(|call| matches!(call, Call::RequestUpgrade(_))),
            "{file_type}: {calls:?}"
        );
    }
}

#[tokio::test]
async fn copied_upgrade_version_requests_nothing() {
    // The copy into a versioned key fires after the type was swapped.
    let calls = handle(
        Some("pptx"),
        DocumentContent::ready(DocumentContentLocation::ObjectStorage),
        versioned_key(99),
    )
    .await;

    assert!(calls.is_empty(), "{calls:?}");
}

#[tokio::test]
async fn upgraded_presentation_becomes_the_current_version() {
    let calls = handle(
        Some("ppt"),
        DocumentContent::ready(DocumentContentLocation::ObjectStorage),
        upgraded_key("pptx"),
    )
    .await;

    assert_eq!(
        calls,
        vec![
            Call::CreateInstance,
            Call::Swap(FileType::Ppt, FileType::Pptx),
            Call::Promote(
                FileType::Pptx,
                UpgradedObjectDestination::DocumentVersion { version_id: 99 }
            ),
            Call::SetContent(
                DocumentContentState::Ready,
                Some(DocumentContentLocation::ObjectStorage)
            ),
        ]
    );
}

#[tokio::test]
async fn upgraded_word_document_is_staged_for_the_docx_pipeline() {
    let calls = handle(
        Some("doc"),
        DocumentContent::ready(DocumentContentLocation::ObjectStorage),
        upgraded_key("docx"),
    )
    .await;

    assert_eq!(
        calls,
        vec![
            Call::CreateBom,
            Call::Swap(FileType::Doc, FileType::Docx),
            Call::SetContent(
                DocumentContentState::Pending,
                Some(DocumentContentLocation::ConvertedPdf)
            ),
            Call::Promote(
                FileType::Docx,
                UpgradedObjectDestination::DocxStaging { bom_id: 5 }
            ),
        ]
    );
}

#[tokio::test]
async fn redelivered_upgrade_is_skipped() {
    let calls = handle(
        Some("xlsx"),
        DocumentContent::ready(DocumentContentLocation::ObjectStorage),
        upgraded_key("xlsx"),
    )
    .await;

    assert!(calls.is_empty(), "{calls:?}");
}

#[tokio::test]
async fn unparseable_keys_are_ignored() {
    for key in [
        format!("{OWNER}/{DOCUMENT_ID}/upgraded.doc"),
        format!("{OWNER}/{DOCUMENT_ID}/notes.txt"),
    ] {
        let calls = handle(Some("doc"), DocumentContent::pending(), key.clone()).await;
        assert!(calls.is_empty(), "{key}");
    }
}

struct CaptureMarkdown(Mutex<Vec<String>>);

async fn receive_m5_document_event(
    sqs: &aws_sdk_sqs::Client,
    queue_url: &str,
) -> anyhow::Result<aws_sdk_sqs::types::Message> {
    for _ in 0..20 {
        let response = sqs
            .receive_message()
            .queue_url(queue_url)
            .wait_time_seconds(1)
            .send()
            .await?;
        if let Some(message) = response.messages.unwrap_or_default().into_iter().next() {
            return Ok(message);
        }
    }
    anyhow::bail!("document upload event did not reach fog-aws")
}

impl MarkdownInitializationPort for CaptureMarkdown {
    async fn initialize_existing_markdown(
        &self,
        _document_id: &str,
        markdown: &str,
    ) -> Result<Vec<u8>, DocumentError> {
        self.0.lock().unwrap().push(markdown.to_string());
        Ok(Vec::new())
    }
}

#[sqlx::test(
    migrator = "macro_db_migrator::MACRO_DB_MIGRATIONS",
    fixtures(
        path = "../../../../crates/documents/fixtures",
        scripts("documents_test_data")
    )
)]
#[ignore = "requires isolated M5 PostgreSQL and RustFS"]
async fn m5_document_upload_retries_then_finalizes_once(pool: sqlx::PgPool) -> anyhow::Result<()> {
    use crate::outbound::{PgDocumentUploadPort, S3DocumentObjectReader};
    use aws_sdk_s3::primitives::ByteStream;
    use bots::outbound::pg_bots_repo::PgBotsRepo;
    use documents::outbound::pg_document_repo::PgDocumentRepo;
    use entity_registry::OwnerGrantPolicy;
    use entity_registry_db_utils::OwnedEntityRegistrar;
    use sqlx::Row;

    let endpoint = std::env::var("FOG_M5_S3_ENDPOINT")?;
    let fog_aws_endpoint = std::env::var("FOG_AWS_TEST_ENDPOINT")?;
    let sqs_config = macro_aws_config::local_aws_config(&fog_aws_endpoint).await;
    let sqs = aws_sdk_sqs::Client::new(&sqs_config);
    let queue_url = format!("{fog_aws_endpoint}/000000000000/document-upload-finalizer-queue");
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let s3 = macro_aws_config::s3_client().await;
    let document_id = "01a0914f-1fde-7873-80ba-6eef315d3b51";
    let key = format!("macro|user@user.com/{document_id}/1");
    let markdown = "# M5 fresh database upload\n";
    s3.put_object()
        .bucket("doc-storage")
        .key(&key)
        .body(ByteStream::from(markdown.as_bytes().to_vec()))
        .send()
        .await?;

    let registrar = OwnedEntityRegistrar::new(OwnerGrantPolicy::new(PgBotsRepo::new(pool.clone())));
    let repo = PgDocumentRepo::new(pool.clone(), registrar);
    let log = Log::default();
    let finalizer = DocumentUploadFinalizer::new(
        PgDocumentUploadPort::new(repo),
        S3DocumentObjectReader::new(s3.clone()),
        FakeUpgradeStorage { log: &log },
    );
    let initializer = CaptureMarkdown(Mutex::new(Vec::new()));
    let message = receive_m5_document_event(&sqs, &queue_url).await?;
    let body = message
        .body()
        .ok_or_else(|| anyhow::anyhow!("event has no body"))?
        .to_owned();
    let events = crate::inbound::s3_notification::object_created_events_from_body(&body)?;
    assert_eq!(events.len(), 1);
    let event = events[0].clone();
    assert_eq!(event.bucket, "doc-storage");
    assert_eq!(event.key, key);
    assert!(
        finalizer
            .handle_object_created(event.clone(), &initializer)
            .await
            .is_err()
    );
    sqs.change_message_visibility()
        .queue_url(&queue_url)
        .receipt_handle(message.receipt_handle().unwrap())
        .visibility_timeout(0)
        .send()
        .await?;

    sqlx::query(
        r#"INSERT INTO "Document" (id, name, "fileType", owner, "uploaded", "contentState")
           VALUES ($1, 'M5 upload', 'md', 'macro|user@user.com', false, 'pending')"#,
    )
    .bind(document_id)
    .execute(&pool)
    .await?;
    let redelivery = receive_m5_document_event(&sqs, &queue_url).await?;
    let redelivered_body = redelivery
        .body()
        .ok_or_else(|| anyhow::anyhow!("redelivery has no body"))?;
    let redelivered =
        crate::inbound::s3_notification::object_created_events_from_body(redelivered_body)?;
    assert_eq!(redelivered, vec![event.clone()]);
    finalizer
        .handle_object_created(event.clone(), &initializer)
        .await?;
    sqs.delete_message()
        .queue_url(&queue_url)
        .receipt_handle(redelivery.receipt_handle().unwrap())
        .send()
        .await?;
    sqs.send_message()
        .queue_url(&queue_url)
        .message_body(body)
        .send()
        .await?;
    let duplicate = receive_m5_document_event(&sqs, &queue_url).await?;
    finalizer.handle_object_created(event, &initializer).await?;
    sqs.delete_message()
        .queue_url(&queue_url)
        .receipt_handle(duplicate.receipt_handle().unwrap())
        .send()
        .await?;

    let row = sqlx::query(
        r#"SELECT "uploaded", "contentState", "contentLocation" FROM "Document" WHERE id = $1"#,
    )
    .bind(document_id)
    .fetch_one(&pool)
    .await?;
    assert!(row.try_get::<bool, _>("uploaded")?);
    assert_eq!(row.try_get::<String, _>("contentState")?, "ready");
    assert_eq!(row.try_get::<String, _>("contentLocation")?, "sync_service");
    assert_eq!(initializer.0.lock().unwrap().as_slice(), &[markdown]);
    assert!(log.calls().is_empty());
    Ok(())
}
