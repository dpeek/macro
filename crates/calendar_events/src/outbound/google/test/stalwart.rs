use super::*;

#[test]
fn default_calendar_client_keeps_google_origin() {
    let client = GoogleCalendarClient::new(Client::new());
    assert_eq!(client.api_base, GOOGLE_CALENDAR_API);
    assert_eq!(client.provider, CalendarProvider::Google);
    let metered = GoogleCalendarClient::with_gate(Client::new(), UnmeteredGate);
    assert_eq!(metered.api_base, GOOGLE_CALENDAR_API);
    assert_eq!(metered.provider, CalendarProvider::Google);
}

#[tokio::test]
async fn stalwart_provider_read_uses_configured_path_and_bearer_token() {
    use axum::{Router, extract::Request, http::StatusCode, routing::any};

    async fn handle(request: Request) -> (StatusCode, &'static str) {
        assert_eq!(request.uri().path(), "/calendar/v3/users/me/calendarList");
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer stalwart-token"
        );
        (StatusCode::OK, r#"{"items":[]}"#)
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_base = format!("http://{}/calendar/v3", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(any(handle)))
            .await
            .unwrap()
    });
    let client = GoogleCalendarClient::with_gate_for_provider(
        Client::new(),
        UnmeteredGate,
        CalendarProvider::Stalwart,
        api_base,
    );
    assert!(
        client
            .list_calendars("stalwart-token", Uuid::now_v7())
            .await
            .unwrap()
            .is_empty()
    );
    task.abort();
}
