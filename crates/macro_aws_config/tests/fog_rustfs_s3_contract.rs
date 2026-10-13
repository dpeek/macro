//! Exercise Macro's standard S3 client against disposable RustFS.

use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::{
    primitives::ByteStream,
    types::{ChecksumMode, Delete, ObjectIdentifier},
};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires disposable RustFS and recorder; use run-rustfs.sh"]
async fn records_rustfs_direct_s3_contracts() -> Result<(), Box<dyn std::error::Error>> {
    // S3 can use fog-aws's streaming listener while other AWS clients use
    // its JSON API listener; this contract must not require equal endpoints.
    let _endpoint = std::env::var("AWS_ENDPOINT_URL_S3")?;
    let s3 = macro_aws_config::s3_client().await;
    let bucket = "fog-contract-direct-s3";
    s3.create_bucket().bucket(bucket).send().await?;
    s3.head_bucket().bucket(bucket).send().await?;
    s3.put_bucket_cors()
        .bucket(bucket)
        .cors_configuration(
            aws_sdk_s3::types::CorsConfiguration::builder()
                .cors_rules(
                    aws_sdk_s3::types::CorsRule::builder()
                        .allowed_methods("PUT")
                        .allowed_methods("GET")
                        .allowed_origins("http://contract.example")
                        .allowed_headers("*")
                        .build()?,
                )
                .build()?,
        )
        .send()
        .await?;

    // Agent/GitHub adapters send an ordinary SDK PUT and read/delete by key.
    let agent_key = "agent-sessions/synthetic/changes/patch.diff";
    let patch = b"diff --git a/a b/a\n+synthetic patch\n";
    s3.put_object()
        .bucket(bucket)
        .key(agent_key)
        .content_type("text/x-diff; charset=utf-8")
        .body(ByteStream::from(patch.to_vec()))
        .send()
        .await?;
    let downloaded = s3.get_object().bucket(bucket).key(agent_key).send().await?;
    assert_eq!(
        downloaded.body.collect().await?.into_bytes().as_ref(),
        patch
    );

    // Slack's browser grant signs content type, length, checksum and a
    // create-only condition. Its verifier HEADs with checksum mode enabled.
    let slack_key = "slack-import/users.json";
    let body = b"{\"synthetic\":true}";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let signed = s3
        .put_object()
        .bucket(bucket)
        .key(slack_key)
        .content_type("application/json")
        .content_length(body.len() as i64)
        .checksum_sha256(&checksum)
        .if_none_match("*")
        .presigned(PresigningConfig::expires_in(Duration::from_secs(120))?)
        .await?;
    let browser = reqwest::Client::new();
    let preflight = browser
        .request(reqwest::Method::OPTIONS, signed.uri())
        .header("origin", "http://contract.example")
        .header("access-control-request-method", "PUT")
        .header(
            "access-control-request-headers",
            "content-type,x-amz-checksum-sha256,if-none-match",
        )
        .send()
        .await?;
    assert!(
        preflight.status().is_success(),
        "RustFS CORS preflight: {}",
        preflight.status()
    );
    let mut upload = browser.put(signed.uri());
    for (name, value) in signed.headers() {
        upload = upload.header(name, value);
    }
    let uploaded = upload.body(body.to_vec()).send().await?;
    assert!(
        uploaded.status().is_success(),
        "Slack-style PUT: {}",
        uploaded.status()
    );
    let mut duplicate = browser.put(signed.uri());
    for (name, value) in signed.headers() {
        duplicate = duplicate.header(name, value);
    }
    let duplicate = duplicate.body(body.to_vec()).send().await?;
    assert_eq!(duplicate.status(), reqwest::StatusCode::PRECONDITION_FAILED);
    let bad_checksum_key = "slack-import/bad-checksum.json";
    let bad_checksum = s3
        .put_object()
        .bucket(bucket)
        .key(bad_checksum_key)
        .content_type("application/json")
        .checksum_sha256(&checksum)
        .body(ByteStream::from(b"different body".to_vec()))
        .send()
        .await;
    assert!(bad_checksum.is_err(), "bad checksum must be rejected");
    let head = s3
        .head_object()
        .bucket(bucket)
        .key(slack_key)
        .checksum_mode(ChecksumMode::Enabled)
        .send()
        .await?;
    assert_eq!(head.content_length(), Some(body.len() as i64));
    assert_eq!(head.checksum_sha256(), Some(checksum.as_str()));
    let etag = head.e_tag().expect("Slack ETag");
    let pinned = s3
        .get_object()
        .bucket(bucket)
        .key(slack_key)
        .checksum_mode(ChecksumMode::Enabled)
        .if_match(etag)
        .send()
        .await?;
    assert_eq!(pinned.checksum_sha256(), Some(checksum.as_str()));
    assert_eq!(pinned.body.collect().await?.into_bytes().as_ref(), body);
    let signed_get = s3
        .get_object()
        .bucket(bucket)
        .key(slack_key)
        .presigned(PresigningConfig::expires_in(Duration::from_secs(120))?)
        .await?;
    let browser_get = browser.get(signed_get.uri()).send().await?;
    assert!(browser_get.status().is_success());
    assert_eq!(browser_get.bytes().await?.as_ref(), body);
    let mut invalid_url = url::Url::parse(signed_get.uri())?;
    invalid_url
        .query_pairs_mut()
        .append_pair("unexpected", "value");
    let invalid_signature = browser.get(invalid_url).send().await?;
    assert_eq!(invalid_signature.status(), reqwest::StatusCode::FORBIDDEN);
    let stale = s3
        .get_object()
        .bucket(bucket)
        .key(slack_key)
        .if_match("\"obsolete\"")
        .send()
        .await
        .expect_err("stale ETag must fail");
    assert_eq!(stale.raw_response().map(|r| r.status().as_u16()), Some(412));
    let missing = s3
        .get_object()
        .bucket(bucket)
        .key("missing/patch.diff")
        .send()
        .await
        .expect_err("missing key must fail");
    assert!(
        missing
            .as_service_error()
            .is_some_and(|error| error.is_no_such_key())
    );

    // Document storage promotes a temporary object, lists by prefix and
    // removes groups in one DeleteObjects call.
    let copied_key = "document/promoted/patch.diff";
    s3.copy_object()
        .bucket(bucket)
        .copy_source(format!("{bucket}/{agent_key}"))
        .key(copied_key)
        .send()
        .await?;
    let copied = s3
        .get_object()
        .bucket(bucket)
        .key(copied_key)
        .send()
        .await?;
    assert_eq!(copied.body.collect().await?.into_bytes().as_ref(), patch);
    let copied_key_two = "document/promoted/second.diff";
    s3.copy_object()
        .bucket(bucket)
        .copy_source(format!("{bucket}/{agent_key}"))
        .key(copied_key_two)
        .send()
        .await?;
    let listed = s3
        .list_objects_v2()
        .bucket(bucket)
        .prefix("document/")
        .max_keys(1)
        .send()
        .await?;
    assert_eq!(listed.contents().len(), 1);
    assert_eq!(listed.contents()[0].key(), Some(copied_key));
    let next = s3
        .list_objects_v2()
        .bucket(bucket)
        .prefix("document/")
        .continuation_token(listed.next_continuation_token().expect("S3 page token"))
        .send()
        .await?;
    assert_eq!(next.contents().len(), 1);
    assert_eq!(next.contents()[0].key(), Some(copied_key_two));
    let deleted = s3
        .delete_objects()
        .bucket(bucket)
        .delete(
            Delete::builder()
                .objects(ObjectIdentifier::builder().key(copied_key).build()?)
                .objects(ObjectIdentifier::builder().key(copied_key_two).build()?)
                .build()?,
        )
        .send()
        .await?;
    assert!(deleted.errors().is_empty());
    s3.delete_object()
        .bucket(bucket)
        .key(agent_key)
        .send()
        .await?;
    s3.delete_object()
        .bucket(bucket)
        .key(slack_key)
        .send()
        .await?;
    Ok(())
}
