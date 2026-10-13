//! Exercise the finalizer's real S3 readers and legacy Office promotion.

use aws_sdk_s3::primitives::ByteStream;
use document_upload_finalizer_handler::outbound::S3LegacyOfficeUpgradeStorage;
use documents::{
    domain::legacy_office_upgrade::{LegacyOfficeUpgradeStoragePort, UpgradedObjectDestination},
    outbound::s3_utf8_object_reader::{S3Utf8ObjectReadError, S3Utf8ObjectReader},
};
use model::document::FileType;
use model_owner::Owner;
use s3_key::{UpgradedOfficeFormat, build_upgraded_office_document_key};
use sha2::{Digest, Sha256};

#[path = "../../image_optimizer/src/s3.rs"]
mod image_storage;
#[path = "../../document_text_extractor/src/service/s3/get_document_bytes.rs"]
mod text_reader;

#[tokio::test]
#[ignore = "requires disposable RustFS; use run-remaining-callers.sh"]
async fn finalizer_s3_adapter_uses_rustfs() -> anyhow::Result<()> {
    let endpoint = std::env::var("FOG_DIRECT_S3_PROXY_URL")?;
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let s3 = macro_aws_config::s3_client().await;
    for bucket in ["doc-storage", "docx-upload", "static-file-storage"] {
        s3.create_bucket().bucket(bucket).send().await?;
    }
    let owner = Owner::from_principal_str("macro|synthetic@example.test")?;
    let document_id = "synthetic-upgrade";
    let source =
        build_upgraded_office_document_key(&owner, document_id, UpgradedOfficeFormat::Docx);
    let body = b"synthetic upgraded bytes";
    let reader = S3Utf8ObjectReader::new(s3.clone());
    assert!(matches!(
        reader.read_utf8("doc-storage", &source).await,
        Err(S3Utf8ObjectReadError::Missing)
    ));
    s3.put_object()
        .bucket("doc-storage")
        .key(&source)
        .content_type("text/plain")
        .body(ByteStream::from_static(body))
        .send()
        .await?;
    assert_eq!(
        reader.read_utf8("doc-storage", &source).await?,
        "synthetic upgraded bytes"
    );
    assert_eq!(
        text_reader::get_document_bytes(&s3, "doc-storage", &source).await?,
        body
    );

    let storage = S3LegacyOfficeUpgradeStorage::new(
        s3.clone(),
        sqs_client::SQS::new(aws_sdk_sqs::Client::new(
            &macro_aws_config::get_macro_aws_config().await,
        )),
        "doc-storage".to_owned(),
        "docx-upload".to_owned(),
    );
    assert_eq!(
        storage
            .upgraded_object_sha(&owner, document_id, FileType::Docx)
            .await?,
        format!("{:x}", Sha256::digest(body))
    );
    for (destination, bucket, suffix) in [
        (
            UpgradedObjectDestination::DocumentVersion { version_id: 2 },
            "doc-storage",
            "2",
        ),
        (
            UpgradedObjectDestination::DocxStaging { bom_id: 3 },
            "docx-upload",
            "3.docx",
        ),
    ] {
        storage
            .promote_upgraded_object(&owner, document_id, FileType::Docx, destination)
            .await?;
        let key = format!("{owner}/{document_id}/{suffix}");
        let object = s3.get_object().bucket(bucket).key(key).send().await?;
        assert_eq!(object.content_type(), Some(FileType::Docx.mime_type()));
        assert_eq!(object.body.collect().await?.into_bytes().as_ref(), body);
    }
    image_storage::store(
        &s3,
        "static-file-storage",
        "synthetic/optimized.jpg",
        b"synthetic image bytes".to_vec(),
        "image/jpeg",
        "3600",
    )
    .await?;
    assert_eq!(
        image_storage::fetch(&s3, "static-file-storage", "synthetic/optimized.jpg").await?,
        (
            b"synthetic image bytes".to_vec(),
            Some("image/jpeg".to_owned())
        )
    );
    let optimized = s3
        .head_object()
        .bucket("static-file-storage")
        .key("synthetic/optimized.jpg")
        .send()
        .await?;
    assert_eq!(optimized.cache_control(), Some("public, max-age=3600"));
    Ok(())
}
