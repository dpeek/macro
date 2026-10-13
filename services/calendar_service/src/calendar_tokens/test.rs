use super::*;
use axum::{Router, extract::Request, http::StatusCode, response::IntoResponse, routing::any};

#[tokio::test]
async fn stalwart_token_requires_the_calendar_scope_from_the_trusted_refresh_response() {
    let identity = CalendarLinkTokenIdentity {
        fusionauth_user_id: "user-id".into(),
        email_address: "person@example.test".into(),
        provider: "STALWART".into(),
    };
    for (status, scope, expected) in [
        (
            StatusCode::OK,
            Some("urn:ietf:params:oauth:scope:calendars"),
            "ok",
        ),
        (
            StatusCode::OK,
            Some("urn:ietf:params:oauth:scope:mail"),
            "reauth",
        ),
        (StatusCode::OK, None, "reauth"),
        (StatusCode::FORBIDDEN, None, "reauth"),
        (StatusCode::INTERNAL_SERVER_ERROR, None, "transient"),
    ] {
        let app = Router::new().fallback(any(move |request: Request| async move {
            assert_eq!(request.uri().path(), "/internal/stalwart_access_token");
            let query = request.uri().query().unwrap();
            assert!(query.contains("fusionauth_user_id=user-id"));
            assert!(query.contains("email=person%40example.test"));
            (
                status,
                axum::Json(serde_json::json!({
                    "access_token": "calendar-token",
                    "expires_in": 3600,
                    "scope": scope,
                })),
            )
                .into_response()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let auth = AuthServiceClient::new("internal-key".into(), url);
        let result = fetch_stalwart_calendar_token(&auth, &identity).await;
        match expected {
            "ok" => assert_eq!(result.unwrap(), "calendar-token"),
            "reauth" => assert!(matches!(result, Err(CalendarTokenError::ReauthRequired(_)))),
            _ => assert!(matches!(result, Err(CalendarTokenError::Transient(_)))),
        }
        task.abort();
    }
}
