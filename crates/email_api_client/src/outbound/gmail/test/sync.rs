use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::repository;
use crate::GmailApiClientRepository;
use crate::domain::models::{AccessToken, EmailApiError, SyncCursor};
use crate::domain::ports::MailboxSyncClient;
use gmail_client::GmailClient;
use models_email::email::service::link::UserProvider;
use wiremock::MockServer;

#[tokio::test]
async fn provider_token_routes_to_its_own_mailbox_endpoint() {
    let google = MockServer::start().await;
    let stalwart = MockServer::start().await;
    for (server, count) in [(&google, 42), (&stalwart, 7)] {
        Mock::given(method("GET"))
            .and(path("/users/me/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "emailAddress": "user@example.test",
                "messagesTotal": 10,
                "threadsTotal": count,
                "historyId": "100"
            })))
            .mount(server)
            .await;
    }
    let client = |url: String| {
        GmailClient::new_with_urls(
            "topic".into(),
            url.clone(),
            url.clone(),
            format!("{url}/certs"),
            "audience".into(),
        )
    };
    let repository = GmailApiClientRepository::new(client(google.uri()))
        .with_stalwart_client(client(stalwart.uri()));

    assert_eq!(
        repository
            .get_thread_count(&AccessToken::new("google-token"))
            .await
            .unwrap(),
        42
    );
    assert_eq!(
        repository
            .get_thread_count(&AccessToken::for_provider(
                "stalwart-token",
                UserProvider::Stalwart
            ))
            .await
            .unwrap(),
        7
    );
}

#[tokio::test]
async fn stalwart_token_never_falls_back_to_google_endpoint() {
    let (_, repository) = repository().await;
    let result = repository
        .get_thread_count(&AccessToken::for_provider(
            "stalwart-token",
            UserProvider::Stalwart,
        ))
        .await;
    assert!(matches!(result, Err(EmailApiError::Permanent { .. })));
}

#[tokio::test]
async fn reads_profile_thread_count() {
    let (server, repository) = repository().await;
    Mock::given(method("GET"))
        .and(path("/users/me/profile"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(include_str!("fixtures/profile.json"), "application/json"),
        )
        .mount(&server)
        .await;

    assert_eq!(
        repository
            .get_thread_count(&AccessToken::new("token"))
            .await
            .unwrap(),
        42
    );
}

#[tokio::test]
async fn paginates_and_partitions_history() {
    let (server, repository) = repository().await;
    Mock::given(method("GET"))
        .and(path("/users/me/history"))
        .and(query_param("startHistoryId", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/history_page_1.json"),
            "application/json",
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/users/me/history"))
        .and(query_param("pageToken", "next-page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/history_page_2.json"),
            "application/json",
        ))
        .mount(&server)
        .await;

    let batch = repository
        .list_changes(&AccessToken::new("token"), &SyncCursor::gmail("100"))
        .await
        .unwrap();

    assert_eq!(batch.next_cursor.as_str(), "102");
    assert!(batch.changes.message_ids_to_upsert.contains("added"));
    assert!(batch.changes.message_ids_to_delete.contains("deleted"));
    assert!(batch.changes.labels_to_update.contains("labels"));
}

#[tokio::test]
async fn only_history_not_found_is_an_outdated_cursor() {
    let (server, repository) = repository().await;
    Mock::given(method("GET"))
        .and(path("/users/me/history"))
        .respond_with(ResponseTemplate::new(404).set_body_raw(
            include_str!("fixtures/stale_history_error.json"),
            "application/json",
        ))
        .mount(&server)
        .await;

    assert_eq!(
        repository
            .list_changes(&AccessToken::new("token"), &SyncCursor::gmail("stale"))
            .await,
        Err(EmailApiError::OutdatedCursor)
    );
}
