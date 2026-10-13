//! Run through config/macro/local-aws-contract/run.sh or run-rustfs.sh. Each
//! uses a disposable backend behind a shape-only recording proxy.

use aws_sdk_dynamodb::{
    error::ProvideErrorMetadata,
    types::{
        AttributeAction, AttributeValue, AttributeValueUpdate, BillingMode, GlobalSecondaryIndex,
        KeySchemaElement, KeyType, KeysAndAttributes, Projection, ProjectionType, Put, PutRequest,
        ReturnValue, ScalarAttributeType, TransactWriteItem, WriteRequest,
    },
};
use aws_sdk_kms::primitives::Blob;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_sqs::types::{
    MessageAttributeValue, MessageSystemAttributeName, QueueAttributeName,
    SendMessageBatchRequestEntry,
};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers};

fn contract_endpoint() -> String {
    let endpoint = std::env::var("FOG_AWS_CONTRACT_PROXY_URL")
        .expect("run this test with config/macro/local-aws-contract/run.sh");
    let url = url::Url::parse(&endpoint).expect("valid contract endpoint");
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(std::env::var("LOCAL_AWS_URL").unwrap(), endpoint);
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3").unwrap(), endpoint);
    endpoint
}

fn proxy_queue_url(queue: &str, endpoint: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut queue_url = url::Url::parse(queue)?;
    let proxy = url::Url::parse(endpoint)?;
    queue_url.set_host(proxy.host_str())?;
    queue_url
        .set_port(proxy.port())
        .expect("loopback queue URL port");
    Ok(queue_url.to_string())
}

async fn record_dynamodb_contracts(
    dynamodb: &aws_sdk_dynamodb::Client,
) -> Result<(), Box<dyn std::error::Error>> {
    const STATIC: &str = "fog-contract-static-file-metadata";
    const BULK: &str = "fog-contract-bulk-upload";

    assert!(
        dynamodb
            .describe_table()
            .table_name(STATIC)
            .send()
            .await?
            .table()
            .is_some()
    );
    // Static upload completion uses legacy AttributeUpdates; the response must
    // retain typed BOOL rather than turn it into a string or null.
    let uploaded = dynamodb
        .update_item()
        .table_name(STATIC)
        .key("file_id", AttributeValue::S("synthetic-file".into()))
        .attribute_updates(
            "is_uploaded",
            AttributeValueUpdate::builder()
                .action(AttributeAction::Put)
                .value(AttributeValue::Bool(true))
                .build(),
        )
        .return_values(ReturnValue::AllNew)
        .send()
        .await?;
    assert!(matches!(
        uploaded
            .attributes()
            .and_then(|item| item.get("is_uploaded")),
        Some(AttributeValue::Bool(true))
    ));
    let duplicate = dynamodb
        .put_item()
        .table_name(STATIC)
        .item("file_id", AttributeValue::S("synthetic-file".into()))
        .condition_expression("attribute_not_exists(file_id)")
        .send()
        .await
        .expect_err("conditional duplicate must fail");
    assert_eq!(
        duplicate
            .as_service_error()
            .and_then(ProvideErrorMetadata::code),
        Some("ConditionalCheckFailedException")
    );
    let missing_update = dynamodb
        .update_item()
        .table_name(STATIC)
        .key("file_id", AttributeValue::S("absent-file".into()))
        .update_expression("SET is_uploaded = :uploaded")
        .expression_attribute_values(":uploaded", AttributeValue::Bool(true))
        .condition_expression("attribute_exists(file_id)")
        .send()
        .await
        .expect_err("missing conditional update must fail");
    assert_eq!(
        missing_update
            .as_service_error()
            .and_then(ProvideErrorMetadata::code),
        Some("ConditionalCheckFailedException")
    );

    let fresh_put = Put::builder()
        .table_name(STATIC)
        .item("file_id", AttributeValue::S("transaction-new".into()))
        .item("is_uploaded", AttributeValue::Bool(false))
        .build()?;
    let conflicting_put = Put::builder()
        .table_name(STATIC)
        .item("file_id", AttributeValue::S("synthetic-file".into()))
        .condition_expression("attribute_not_exists(file_id)")
        .build()?;
    let canceled = dynamodb
        .transact_write_items()
        .transact_items(TransactWriteItem::builder().put(fresh_put.clone()).build())
        .transact_items(TransactWriteItem::builder().put(conflicting_put).build())
        .send()
        .await
        .expect_err("transaction must cancel as one unit");
    assert_eq!(
        canceled
            .as_service_error()
            .and_then(ProvideErrorMetadata::code),
        Some("TransactionCanceledException")
    );
    assert!(
        dynamodb
            .get_item()
            .table_name(STATIC)
            .key("file_id", AttributeValue::S("transaction-new".into()))
            .send()
            .await?
            .item()
            .is_none(),
        "canceled transaction partly committed"
    );
    dynamodb
        .transact_write_items()
        .transact_items(TransactWriteItem::builder().put(fresh_put).build())
        .transact_items(
            TransactWriteItem::builder()
                .delete(
                    aws_sdk_dynamodb::types::Delete::builder()
                        .table_name(STATIC)
                        .key("file_id", AttributeValue::S("synthetic-file".into()))
                        .build()?,
                )
                .build(),
        )
        .send()
        .await?;
    assert!(
        dynamodb
            .get_item()
            .table_name(STATIC)
            .key("file_id", AttributeValue::S("transaction-new".into()))
            .send()
            .await?
            .item()
            .is_some()
    );
    let missing_table = dynamodb
        .delete_item()
        .table_name("fog-contract-no-such-table")
        .key("file_id", AttributeValue::S("synthetic-file".into()))
        .send()
        .await
        .expect_err("missing table must fail");
    assert_eq!(
        missing_table
            .as_service_error()
            .and_then(ProvideErrorMetadata::code),
        Some("ResourceNotFoundException")
    );

    // The bulk-upload table uses PK/SK and its DocumentPkIndex on SK.
    dynamodb
        .create_table()
        .table_name(BULK)
        .attribute_definitions(
            aws_sdk_dynamodb::types::AttributeDefinition::builder()
                .attribute_name("PK")
                .attribute_type(ScalarAttributeType::S)
                .build()?,
        )
        .attribute_definitions(
            aws_sdk_dynamodb::types::AttributeDefinition::builder()
                .attribute_name("SK")
                .attribute_type(ScalarAttributeType::S)
                .build()?,
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("PK")
                .key_type(KeyType::Hash)
                .build()?,
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("SK")
                .key_type(KeyType::Range)
                .build()?,
        )
        .billing_mode(BillingMode::PayPerRequest)
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("DocumentPkIndex")
                .key_schema(
                    KeySchemaElement::builder()
                        .attribute_name("SK")
                        .key_type(KeyType::Hash)
                        .build()?,
                )
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .build()?,
        )
        .send()
        .await?;
    assert!(
        dynamodb
            .describe_table()
            .table_name(BULK)
            .send()
            .await?
            .table()
            .is_some()
    );
    let writes = ["DOCUMENT#a", "DOCUMENT#b", "DOCUMENT#c"]
        .into_iter()
        .map(|sk| {
            Ok(WriteRequest::builder()
                .put_request(
                    PutRequest::builder()
                        .item("PK", AttributeValue::S("REQUEST#one".into()))
                        .item("SK", AttributeValue::S(sk.into()))
                        .item("status", AttributeValue::S("PENDING".into()))
                        .build()?,
                )
                .build())
        })
        .collect::<Result<Vec<_>, aws_sdk_dynamodb::error::BuildError>>()?;
    let batch_write = dynamodb
        .batch_write_item()
        .request_items(BULK, writes)
        .send()
        .await?;
    assert!(
        batch_write
            .unprocessed_items()
            .is_none_or(|items| items.is_empty())
    );
    let keys = ["DOCUMENT#a", "DOCUMENT#b"]
        .into_iter()
        .map(|sk| {
            [
                ("PK".to_owned(), AttributeValue::S("REQUEST#one".into())),
                ("SK".to_owned(), AttributeValue::S(sk.into())),
            ]
            .into()
        })
        .collect::<Vec<_>>();
    let batch_get = dynamodb
        .batch_get_item()
        .request_items(
            BULK,
            KeysAndAttributes::builder()
                .set_keys(Some(keys))
                .consistent_read(true)
                .build()?,
        )
        .send()
        .await?;
    assert_eq!(
        batch_get
            .responses()
            .and_then(|tables| tables.get(BULK))
            .map(Vec::len),
        Some(2)
    );
    assert!(
        batch_get
            .unprocessed_keys()
            .is_none_or(|keys| keys.is_empty())
    );

    let first_page = dynamodb
        .query()
        .table_name(BULK)
        .key_condition_expression("PK = :pk")
        .projection_expression("SK, #status")
        .expression_attribute_names("#status", "status")
        .expression_attribute_values(":pk", AttributeValue::S("REQUEST#one".into()))
        .consistent_read(true)
        .limit(2)
        .send()
        .await?;
    assert_eq!(first_page.items().len(), 2);
    let page_key = first_page
        .last_evaluated_key()
        .expect("query continuation key");
    let second_page = dynamodb
        .query()
        .table_name(BULK)
        .key_condition_expression("PK = :pk")
        .projection_expression("SK, #status")
        .expression_attribute_names("#status", "status")
        .expression_attribute_values(":pk", AttributeValue::S("REQUEST#one".into()))
        .consistent_read(true)
        .limit(2)
        .set_exclusive_start_key(Some(page_key.clone()))
        .send()
        .await?;
    assert_eq!(second_page.items().len(), 1);
    assert!(second_page.last_evaluated_key().is_none());
    let by_document = dynamodb
        .query()
        .table_name(BULK)
        .index_name("DocumentPkIndex")
        .key_condition_expression("SK = :sk")
        .expression_attribute_values(":sk", AttributeValue::S("DOCUMENT#a".into()))
        .send()
        .await?;
    assert_eq!(by_document.items().len(), 1);
    let scan = dynamodb.scan().table_name(BULK).limit(2).send().await?;
    assert_eq!(scan.items().len(), 2);
    assert!(scan.last_evaluated_key().is_some());

    let updated = dynamodb
        .update_item()
        .table_name(BULK)
        .key("PK", AttributeValue::S("REQUEST#one".into()))
        .key("SK", AttributeValue::S("DOCUMENT#a".into()))
        .update_expression("SET #status = :status, #error = :error")
        .expression_attribute_names("#status", "status")
        .expression_attribute_names("#error", "error_message")
        .expression_attribute_values(":status", AttributeValue::S("COMPLETE".into()))
        .expression_attribute_values(":error", AttributeValue::Null(true))
        .return_values(ReturnValue::AllNew)
        .send()
        .await?;
    assert!(matches!(
        updated
            .attributes()
            .and_then(|item| item.get("error_message")),
        Some(AttributeValue::Null(true))
    ));
    dynamodb
        .delete_item()
        .table_name(BULK)
        .key("PK", AttributeValue::S("REQUEST#one".into()))
        .key("SK", AttributeValue::S("DOCUMENT#c".into()))
        .send()
        .await?;
    Ok(())
}

async fn record_connection_gateway_contracts(
    dynamodb: &aws_sdk_dynamodb::Client,
) -> Result<(), Box<dyn std::error::Error>> {
    const TABLE: &str = "fog-contract-connection-gateway";
    dynamodb
        .create_table()
        .table_name(TABLE)
        .attribute_definitions(
            aws_sdk_dynamodb::types::AttributeDefinition::builder()
                .attribute_name("PK")
                .attribute_type(ScalarAttributeType::S)
                .build()?,
        )
        .attribute_definitions(
            aws_sdk_dynamodb::types::AttributeDefinition::builder()
                .attribute_name("SK")
                .attribute_type(ScalarAttributeType::S)
                .build()?,
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("PK")
                .key_type(KeyType::Hash)
                .build()?,
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("SK")
                .key_type(KeyType::Range)
                .build()?,
        )
        .billing_mode(BillingMode::PayPerRequest)
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("ConnectionPkIndex")
                .key_schema(
                    KeySchemaElement::builder()
                        .attribute_name("SK")
                        .key_type(KeyType::Hash)
                        .build()?,
                )
                .key_schema(
                    KeySchemaElement::builder()
                        .attribute_name("PK")
                        .key_type(KeyType::Range)
                        .build()?,
                )
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .build()?,
        )
        .send()
        .await?;
    for pk in ["#User#alice", "#User#bob", "#Organization#team"] {
        dynamodb
            .put_item()
            .table_name(TABLE)
            .item("PK", AttributeValue::S(pk.into()))
            .item("SK", AttributeValue::S("connection-one".into()))
            .item("entity_type", AttributeValue::S("synthetic".into()))
            .item("last_ping", AttributeValue::N("1".into()))
            .send()
            .await?;
    }
    let by_entity = dynamodb
        .query()
        .table_name(TABLE)
        .key_condition_expression("PK = :primary_key")
        .expression_attribute_values(":primary_key", AttributeValue::S("#User#alice".into()))
        .send()
        .await?;
    assert_eq!(by_entity.items().len(), 1);
    let by_connection = dynamodb
        .query()
        .table_name(TABLE)
        .index_name("ConnectionPkIndex")
        .key_condition_expression("SK = :connection_id")
        .expression_attribute_values(":connection_id", AttributeValue::S("connection-one".into()))
        .send()
        .await?;
    assert_eq!(by_connection.items().len(), 3);
    let exact = dynamodb
        .query()
        .table_name(TABLE)
        .key_condition_expression("PK = :primary_key AND SK = :connection_id")
        .consistent_read(true)
        .expression_attribute_values(":primary_key", AttributeValue::S("#User#alice".into()))
        .expression_attribute_values(":connection_id", AttributeValue::S("connection-one".into()))
        .send()
        .await?;
    assert_eq!(exact.items().len(), 1);

    // Macro's former expression had no partition-key equality. AWS documents
    // this as invalid; record whether the local reference backend rejects it.
    let invalid = dynamodb
        .query()
        .table_name(TABLE)
        .key_condition_expression("begins_with(PK, :user_prefix) AND SK = :connection_id")
        .expression_attribute_values(":user_prefix", AttributeValue::S("#User".into()))
        .expression_attribute_values(":connection_id", AttributeValue::S("connection-one".into()))
        .send()
        .await
        .expect_err("partition key prefix query must be rejected");
    assert_eq!(
        invalid
            .as_service_error()
            .and_then(ProvideErrorMetadata::code),
        Some("ValidationException")
    );
    // The index reverses the keys: SK is its partition key and PK its sort
    // key, so the same lookup can use begins_with legally.
    let users = dynamodb
        .query()
        .table_name(TABLE)
        .index_name("ConnectionPkIndex")
        .key_condition_expression("SK = :connection_id AND begins_with(PK, :user_prefix)")
        .expression_attribute_values(":connection_id", AttributeValue::S("connection-one".into()))
        .expression_attribute_values(":user_prefix", AttributeValue::S("#User".into()))
        .send()
        .await?;
    assert_eq!(users.items().len(), 2);

    let ping = dynamodb
        .update_item()
        .table_name(TABLE)
        .key("PK", AttributeValue::S("#User#alice".into()))
        .key("SK", AttributeValue::S("connection-one".into()))
        .update_expression("SET #last_ping = :timestamp")
        .condition_expression("attribute_exists(#entity_type)")
        .expression_attribute_names("#last_ping", "last_ping")
        .expression_attribute_names("#entity_type", "entity_type")
        .expression_attribute_values(":timestamp", AttributeValue::N("2".into()))
        .return_values(ReturnValue::AllNew)
        .send()
        .await?;
    assert!(matches!(
        ping.attributes().and_then(|attrs| attrs.get("last_ping")),
        Some(AttributeValue::N(value)) if value == "2"
    ));
    let deleted_ping = dynamodb
        .update_item()
        .table_name(TABLE)
        .key("PK", AttributeValue::S("#User#removed".into()))
        .key("SK", AttributeValue::S("connection-one".into()))
        .update_expression("SET #last_ping = :timestamp")
        .condition_expression("attribute_exists(#entity_type)")
        .expression_attribute_names("#last_ping", "last_ping")
        .expression_attribute_names("#entity_type", "entity_type")
        .expression_attribute_values(":timestamp", AttributeValue::N("2".into()))
        .send()
        .await
        .expect_err("deleted connection must reject ping");
    assert_eq!(
        deleted_ping
            .as_service_error()
            .and_then(ProvideErrorMetadata::code),
        Some("ConditionalCheckFailedException")
    );
    Ok(())
}

async fn check_unprocessed_batch_responses() -> Result<(), Box<dyn std::error::Error>> {
    // LocalStack normally processes these tiny batches in full. Model AWS's
    // documented partial-success responses to verify SDK decoding and the
    // caller's required retry with only the returned subset.
    const TABLE: &str = "fog-contract-partial";
    let server = MockServer::start().await;
    let write_calls = Arc::new(AtomicUsize::new(0));
    let write_counter = write_calls.clone();
    Mock::given(matchers::method("POST"))
        .and(matchers::header(
            "x-amz-target",
            "DynamoDB_20120810.BatchWriteItem",
        ))
        .respond_with(move |request: &wiremock::Request| {
            let call = write_counter.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("SDK batch JSON");
                let one = body["RequestItems"][TABLE][0].clone();
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"UnprocessedItems": {"fog-contract-partial": [one]}}),
                )
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"UnprocessedItems": {}}))
            }
        })
        .mount(&server)
        .await;
    let get_calls = Arc::new(AtomicUsize::new(0));
    let get_counter = get_calls.clone();
    Mock::given(matchers::method("POST"))
        .and(matchers::header(
            "x-amz-target",
            "DynamoDB_20120810.BatchGetItem",
        ))
        .respond_with(move |request: &wiremock::Request| {
            let call = get_counter.fetch_add(1, Ordering::SeqCst);
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("SDK batch JSON");
            if call == 0 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "UnprocessedKeys": {"fog-contract-partial": body["RequestItems"][TABLE]}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "Responses": {"fog-contract-partial": [body["RequestItems"][TABLE]["Keys"][0]]},
                    "UnprocessedKeys": {}
                }))
            }
        })
        .mount(&server)
        .await;
    let config = macro_aws_config::local_aws_config(&server.uri()).await;
    let client = aws_sdk_dynamodb::Client::new(&config);
    let write_entry = |sk: &str| -> Result<WriteRequest, aws_sdk_dynamodb::error::BuildError> {
        Ok(WriteRequest::builder()
            .put_request(
                PutRequest::builder()
                    .item("PK", AttributeValue::S("REQUEST#partial".into()))
                    .item("SK", AttributeValue::S(sk.into()))
                    .build()?,
            )
            .build())
    };
    let first = client
        .batch_write_item()
        .request_items(
            TABLE,
            vec![write_entry("DOCUMENT#a")?, write_entry("DOCUMENT#b")?],
        )
        .send()
        .await?;
    let unprocessed = first.unprocessed_items().expect("unprocessed write map");
    assert_eq!(unprocessed.get(TABLE).map(Vec::len), Some(1));
    let second = client
        .batch_write_item()
        .set_request_items(Some(unprocessed.clone()))
        .send()
        .await?;
    assert!(
        second
            .unprocessed_items()
            .is_none_or(|items| items.is_empty())
    );
    assert_eq!(write_calls.load(Ordering::SeqCst), 2);

    let key = [
        ("PK".to_owned(), AttributeValue::S("REQUEST#partial".into())),
        ("SK".to_owned(), AttributeValue::S("DOCUMENT#a".into())),
    ]
    .into();
    let first = client
        .batch_get_item()
        .request_items(TABLE, KeysAndAttributes::builder().keys(key).build()?)
        .send()
        .await?;
    let unprocessed = first.unprocessed_keys().expect("unprocessed key map");
    assert_eq!(
        unprocessed.get(TABLE).map(|keys| keys.keys().len()),
        Some(1)
    );
    let second = client
        .batch_get_item()
        .set_request_items(Some(unprocessed.clone()))
        .send()
        .await?;
    assert_eq!(
        second
            .responses()
            .and_then(|tables| tables.get(TABLE))
            .map(Vec::len),
        Some(1)
    );
    assert!(second.unprocessed_keys().is_none_or(|keys| keys.is_empty()));
    assert_eq!(get_calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable LocalStack and recorder; use run.sh"]
async fn records_first_macro_aws_contracts() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = contract_endpoint();
    let config = macro_aws_config::get_macro_aws_config().await;
    let s3 = macro_aws_config::s3_client().await;
    let sqs = macro_aws_config::sqs_client().await;
    let dynamodb = aws_sdk_dynamodb::Client::new(&config);
    let kms = aws_sdk_kms::Client::new(&config);

    // Browser upload contracts: the static-file path signs Content-Type;
    // document uploads additionally sign the SHA-256 checksum.
    let bucket = "fog-contract-storage";
    s3.create_bucket().bucket(bucket).send().await?;
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
    let browser = reqwest::Client::new();
    for (key, checksum) in [("static/test.txt", false), ("document/test.txt", true)] {
        let body = b"fog contract payload";
        let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        let mut put = s3
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("text/plain");
        if checksum {
            put = put.checksum_sha256(hash.clone());
        }
        let signed = put
            .presigned(PresigningConfig::expires_in(Duration::from_secs(120))?)
            .await?;
        let preflight = browser
            .request(reqwest::Method::OPTIONS, signed.uri())
            .header("origin", "http://contract.example")
            .header("access-control-request-method", "PUT")
            .header(
                "access-control-request-headers",
                "content-type,x-amz-checksum-sha256",
            )
            .send()
            .await?;
        assert!(
            preflight.status().is_success(),
            "S3 preflight: {}",
            preflight.status()
        );
        let mut request = browser
            .put(signed.uri())
            .header("content-type", "text/plain");
        if checksum {
            request = request.header("x-amz-checksum-sha256", hash);
        }
        let response = request.body(body.to_vec()).send().await?;
        assert!(
            response.status().is_success(),
            "S3 PUT: {}",
            response.status()
        );
        let signed_get = s3
            .get_object()
            .bucket(bucket)
            .key(key)
            .presigned(PresigningConfig::expires_in(Duration::from_secs(120))?)
            .await?;
        let response = browser.get(signed_get.uri()).send().await?;
        assert!(
            response.status().is_success(),
            "S3 GET: {}",
            response.status()
        );
        assert_eq!(response.bytes().await?.as_ref(), body);
    }

    // SQS calls match the generic Macro queue transport's send/receive/delete.
    let queue = sqs
        .create_queue()
        .queue_name("fog-contract-queue")
        .send()
        .await?
        .queue_url
        .expect("queue URL");
    let queue_url = proxy_queue_url(&queue, &endpoint)?;
    sqs.send_message()
        .queue_url(&queue_url)
        .message_body("synthetic message; never recorded")
        .send()
        .await?;
    let received = sqs
        .receive_message()
        .queue_url(&queue_url)
        .max_number_of_messages(1)
        .wait_time_seconds(1)
        .send()
        .await?;
    let receipt = received
        .messages()
        .first()
        .and_then(|message| message.receipt_handle())
        .expect("received synthetic message");
    sqs.delete_message()
        .queue_url(&queue_url)
        .receipt_handle(receipt)
        .send()
        .await?;

    // Slack/document workers need per-entry batch results and typed attributes.
    let attribute = MessageAttributeValue::builder()
        .data_type("String")
        .string_value("synthetic-source")
        .build()?;
    let batch = sqs
        .send_message_batch()
        .queue_url(&queue_url)
        .entries(
            SendMessageBatchRequestEntry::builder()
                .id("first")
                .message_body("synthetic batch one")
                .message_attributes("source", attribute)
                .build()?,
        )
        .entries(
            SendMessageBatchRequestEntry::builder()
                .id("second")
                .message_body("synthetic batch two")
                .build()?,
        )
        .send()
        .await?;
    assert_eq!(batch.successful().len(), 2);
    assert!(batch.failed().is_empty());
    let batch_deliveries = sqs
        .receive_message()
        .queue_url(&queue_url)
        .max_number_of_messages(10)
        .wait_time_seconds(1)
        .message_attribute_names("All")
        .send()
        .await?;
    assert_eq!(batch_deliveries.messages().len(), 2);
    assert!(batch_deliveries.messages().iter().any(|message| {
        message
            .message_attributes()
            .and_then(|attrs| attrs.get("source"))
            .and_then(|attr| attr.string_value())
            == Some("synthetic-source")
    }));
    for message in batch_deliveries.messages() {
        sqs.delete_message()
            .queue_url(&queue_url)
            .receipt_handle(message.receipt_handle().expect("batch receipt"))
            .send()
            .await?;
    }

    // Delay must hide a message; extending or releasing its lease changes
    // which receipt is current and increases ApproximateReceiveCount.
    let lease_queue = sqs
        .create_queue()
        .queue_name("fog-contract-lease-queue")
        .send()
        .await?
        .queue_url
        .expect("lease queue URL");
    let lease_queue = proxy_queue_url(&lease_queue, &endpoint)?;
    sqs.send_message()
        .queue_url(&lease_queue)
        .message_body("synthetic delayed message")
        .delay_seconds(2)
        .send()
        .await?;
    let hidden = sqs
        .receive_message()
        .queue_url(&lease_queue)
        .wait_time_seconds(0)
        .send()
        .await?;
    assert!(hidden.messages().is_empty(), "delay was ignored");
    std::thread::sleep(Duration::from_secs(4));
    let first = sqs
        .receive_message()
        .queue_url(&lease_queue)
        .visibility_timeout(30)
        .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount)
        .send()
        .await?;
    let first = first.messages().first().expect("delayed message visible");
    let first_receipt = first.receipt_handle().expect("first receipt").to_owned();
    sqs.change_message_visibility()
        .queue_url(&lease_queue)
        .receipt_handle(&first_receipt)
        .visibility_timeout(0)
        .send()
        .await?;
    let second = sqs
        .receive_message()
        .queue_url(&lease_queue)
        .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount)
        .send()
        .await?;
    let second = second.messages().first().expect("released message visible");
    let second_receipt = second.receipt_handle().expect("second receipt");
    assert_ne!(first_receipt, second_receipt);
    assert_eq!(
        second
            .attributes()
            .and_then(|attrs| attrs.get(&MessageSystemAttributeName::ApproximateReceiveCount))
            .map(String::as_str),
        Some("2")
    );
    sqs.delete_message()
        .queue_url(&lease_queue)
        .receipt_handle(second_receipt)
        .send()
        .await?;

    // The webhook queue is FIFO. A duplicate deduplication ID must not
    // produce another delivery, and one group retains send order.
    let fifo_queue = sqs
        .create_queue()
        .queue_name("fog-contract-webhook.fifo")
        .attributes(QueueAttributeName::FifoQueue, "true")
        .send()
        .await?
        .queue_url
        .expect("FIFO queue URL");
    let fifo_queue = proxy_queue_url(&fifo_queue, &endpoint)?;
    for (body, dedup) in [("first", "one"), ("duplicate", "one"), ("second", "two")] {
        sqs.send_message()
            .queue_url(&fifo_queue)
            .message_body(body)
            .message_group_id("group-a")
            .message_deduplication_id(dedup)
            .send()
            .await?;
    }
    for expected in ["first", "second"] {
        let delivery = sqs
            .receive_message()
            .queue_url(&fifo_queue)
            .max_number_of_messages(1)
            .send()
            .await?;
        let delivery = delivery.messages().first().expect("FIFO delivery");
        assert_eq!(delivery.body(), Some(expected));
        sqs.delete_message()
            .queue_url(&fifo_queue)
            .receipt_handle(delivery.receipt_handle().expect("FIFO receipt"))
            .send()
            .await?;
    }
    assert!(
        sqs.receive_message()
            .queue_url(&fifo_queue)
            .wait_time_seconds(0)
            .send()
            .await?
            .messages()
            .is_empty(),
        "duplicate FIFO message was delivered"
    );

    // Mirror the Slack import queue's five-receive redrive and 14-day DLQ.
    let dlq = sqs
        .create_queue()
        .queue_name("fog-contract-slack-dlq")
        .attributes(QueueAttributeName::MessageRetentionPeriod, "1209600")
        .send()
        .await?
        .queue_url
        .expect("DLQ URL");
    let dlq = proxy_queue_url(&dlq, &endpoint)?;
    let dlq_arn = sqs
        .get_queue_attributes()
        .queue_url(&dlq)
        .attribute_names(QueueAttributeName::QueueArn)
        .send()
        .await?
        .attributes()
        .and_then(|attrs| attrs.get(&QueueAttributeName::QueueArn))
        .expect("DLQ ARN")
        .to_owned();
    let main = sqs
        .create_queue()
        .queue_name("fog-contract-slack-main")
        .send()
        .await?
        .queue_url
        .expect("main queue URL");
    let main = proxy_queue_url(&main, &endpoint)?;
    sqs.set_queue_attributes()
        .queue_url(&main)
        .attributes(QueueAttributeName::VisibilityTimeout, "1")
        .attributes(
            QueueAttributeName::RedrivePolicy,
            format!(r#"{{"deadLetterTargetArn":"{dlq_arn}","maxReceiveCount":5}}"#),
        )
        .send()
        .await?;
    let _resolved_main = sqs
        .get_queue_url()
        .queue_name("fog-contract-slack-main")
        .send()
        .await?
        .queue_url
        .expect("GetQueueUrl");
    sqs.send_message()
        .queue_url(&main)
        .message_body("synthetic retry to DLQ")
        .send()
        .await?;
    for expected_receive_count in ["1", "2", "3", "4", "5"] {
        let delivery = sqs
            .receive_message()
            .queue_url(&main)
            .visibility_timeout(1)
            .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount)
            .send()
            .await?;
        let delivery = delivery.messages().first().expect("retry delivery");
        assert_eq!(
            delivery
                .attributes()
                .and_then(|attrs| attrs.get(&MessageSystemAttributeName::ApproximateReceiveCount))
                .map(String::as_str),
            Some(expected_receive_count)
        );
        sqs.change_message_visibility()
            .queue_url(&main)
            .receipt_handle(delivery.receipt_handle().expect("retry receipt"))
            .visibility_timeout(0)
            .send()
            .await?;
    }
    let no_more_main = sqs
        .receive_message()
        .queue_url(&main)
        .wait_time_seconds(1)
        .send()
        .await?;
    assert!(no_more_main.messages().is_empty(), "redrive did not occur");
    let dead_letter = sqs
        .receive_message()
        .queue_url(&dlq)
        .wait_time_seconds(1)
        .send()
        .await?;
    assert_eq!(
        dead_letter.messages().first().and_then(|m| m.body()),
        Some("synthetic retry to DLQ")
    );

    // Static-file metadata uses file_id as the table key and BOOL upload state.
    dynamodb
        .create_table()
        .table_name("fog-contract-static-file-metadata")
        .attribute_definitions(
            aws_sdk_dynamodb::types::AttributeDefinition::builder()
                .attribute_name("file_id")
                .attribute_type(ScalarAttributeType::S)
                .build()?,
        )
        .key_schema(
            aws_sdk_dynamodb::types::KeySchemaElement::builder()
                .attribute_name("file_id")
                .key_type(aws_sdk_dynamodb::types::KeyType::Hash)
                .build()?,
        )
        .billing_mode(BillingMode::PayPerRequest)
        .send()
        .await?;
    dynamodb
        .put_item()
        .table_name("fog-contract-static-file-metadata")
        .item("file_id", AttributeValue::S("synthetic-file".into()))
        .item("is_uploaded", AttributeValue::Bool(false))
        .send()
        .await?;
    let item = dynamodb
        .get_item()
        .table_name("fog-contract-static-file-metadata")
        .key("file_id", AttributeValue::S("synthetic-file".into()))
        .send()
        .await?;
    assert!(item.item().is_some());
    record_dynamodb_contracts(&dynamodb).await?;
    record_connection_gateway_contracts(&dynamodb).await?;
    check_unprocessed_batch_responses().await?;

    // Cursor/Codex envelopes bind ciphertext to their exact context.
    let key_id = kms
        .create_key()
        .send()
        .await?
        .key_metadata()
        .map(|metadata| metadata.key_id())
        .expect("KMS key ID")
        .to_owned();
    let ciphertext = kms
        .encrypt()
        .key_id(&key_id)
        .plaintext(Blob::new(b"synthetic secret"))
        .encryption_context("purpose", "fog-contract")
        .send()
        .await?
        .ciphertext_blob
        .expect("KMS ciphertext");
    let plaintext = kms
        .decrypt()
        .key_id(&key_id)
        .ciphertext_blob(ciphertext.clone())
        .encryption_context("purpose", "fog-contract")
        .send()
        .await?
        .plaintext
        .expect("KMS plaintext");
    assert_eq!(plaintext.as_ref(), b"synthetic secret");
    let wrong_context = kms
        .decrypt()
        .key_id(&key_id)
        .ciphertext_blob(ciphertext)
        .encryption_context("purpose", "wrong")
        .send()
        .await;
    assert!(wrong_context.is_err(), "KMS must reject the wrong context");
    Ok(())
}
