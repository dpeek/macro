//! Route calendar requests by the persisted provider of their email link.

use sqlx::PgPool;
use uuid::Uuid;

use super::google::{GoogleCalendarClient, GoogleRequestGate};
use crate::domain::{
    models::{
        ActorInboxes, AttendeeResponseStatus, CalendarEventDraft, CalendarEventPatch,
        CalendarEventUpsert, CalendarProvider, GoogleEventSyncBatch, GoogleWatchChannel,
        GoogleWatchConfig, ProviderCalendar, ProviderCalendarTarget,
    },
    ports::{
        CalendarMutationProvider, CalendarProviderError, CalendarProviderErrorKind,
        CalendarRsvpScope, GoogleCalendarProvider, GoogleEventSyncContext,
        ProviderInstanceUpdateOutcome, ProviderRsvpOutcome, ProviderSeriesMutationOutcome,
    },
};

/// Two Google-shaped HTTP clients, selected independently for each linked inbox.
pub struct RoutedCalendarClient<G> {
    pool: PgPool,
    google: GoogleCalendarClient<G>,
    stalwart: Option<GoogleCalendarClient<G>>,
}

impl<G: GoogleRequestGate> RoutedCalendarClient<G> {
    /// Construct a router. Stalwart is optional in Google-only deployments.
    pub fn new(
        pool: PgPool,
        google: GoogleCalendarClient<G>,
        stalwart: Option<GoogleCalendarClient<G>>,
    ) -> Self {
        Self {
            pool,
            google,
            stalwart,
        }
    }

    async fn for_link(
        &self,
        link_id: Uuid,
    ) -> Result<&GoogleCalendarClient<G>, CalendarProviderError> {
        let provider: Option<String> =
            sqlx::query_scalar("SELECT provider::text FROM email_links WHERE id = $1")
                .bind(link_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|error| {
                    CalendarProviderError::new(
                        CalendarProviderErrorKind::Transient,
                        error.to_string(),
                    )
                })?;
        match provider
            .as_deref()
            .and_then(CalendarProvider::from_link_provider)
        {
            Some(CalendarProvider::Google) => Ok(&self.google),
            Some(CalendarProvider::Stalwart) => self.stalwart.as_ref().ok_or_else(|| {
                CalendarProviderError::new(
                    CalendarProviderErrorKind::Permanent,
                    "Stalwart calendar API is not configured",
                )
            }),
            Some(CalendarProvider::Outlook) | None => Err(CalendarProviderError::new(
                CalendarProviderErrorKind::Permanent,
                "calendar link has no supported provider",
            )),
        }
    }
}

impl<G: GoogleRequestGate> GoogleCalendarProvider for RoutedCalendarClient<G> {
    async fn list_calendars(
        &self,
        access_token: &str,
        email_link_id: Uuid,
    ) -> Result<Vec<ProviderCalendar>, CalendarProviderError> {
        self.for_link(email_link_id)
            .await?
            .list_calendars(access_token, email_link_id)
            .await
    }

    async fn sync_events(
        &self,
        access_token: &str,
        context: GoogleEventSyncContext,
    ) -> Result<GoogleEventSyncBatch, CalendarProviderError> {
        self.for_link(context.target.email_link_id)
            .await?
            .sync_events(access_token, context)
            .await
    }

    async fn watch_calendar(
        &self,
        access_token: &str,
        email_link_id: Uuid,
        provider_calendar_id: &str,
        channel_id: Uuid,
        config: &GoogleWatchConfig,
    ) -> Result<GoogleWatchChannel, CalendarProviderError> {
        self.for_link(email_link_id)
            .await?
            .watch_calendar(
                access_token,
                email_link_id,
                provider_calendar_id,
                channel_id,
                config,
            )
            .await
    }
}

impl<G: GoogleRequestGate> CalendarMutationProvider for RoutedCalendarClient<G> {
    async fn create_event(
        &self,
        access_token: &str,
        target: &ProviderCalendarTarget,
        draft: &CalendarEventDraft,
    ) -> Result<CalendarEventUpsert, CalendarProviderError> {
        self.for_link(target.email_link_id)
            .await?
            .create_event(access_token, target, draft)
            .await
    }

    async fn update_event(
        &self,
        access_token: &str,
        target: &ProviderCalendarTarget,
        provider_event_id: &str,
        patch: &CalendarEventPatch,
    ) -> Result<Option<CalendarEventUpsert>, CalendarProviderError> {
        self.for_link(target.email_link_id)
            .await?
            .update_event(access_token, target, provider_event_id, patch)
            .await
    }

    async fn update_event_instance(
        &self,
        access_token: &str,
        target: &ProviderCalendarTarget,
        master_provider_event_id: &str,
        original_start: &str,
        patch: &CalendarEventPatch,
    ) -> Result<ProviderInstanceUpdateOutcome, CalendarProviderError> {
        self.for_link(target.email_link_id)
            .await?
            .update_event_instance(
                access_token,
                target,
                master_provider_event_id,
                original_start,
                patch,
            )
            .await
    }

    async fn delete_event(
        &self,
        access_token: &str,
        target: &ProviderCalendarTarget,
        provider_event_id: &str,
    ) -> Result<(), CalendarProviderError> {
        self.for_link(target.email_link_id)
            .await?
            .delete_event(access_token, target, provider_event_id)
            .await
    }

    async fn delete_event_instance(
        &self,
        access_token: &str,
        target: &ProviderCalendarTarget,
        master_provider_event_id: &str,
        original_start: &str,
    ) -> Result<ProviderSeriesMutationOutcome, CalendarProviderError> {
        self.for_link(target.email_link_id)
            .await?
            .delete_event_instance(
                access_token,
                target,
                master_provider_event_id,
                original_start,
            )
            .await
    }

    async fn truncate_recurring_event(
        &self,
        access_token: &str,
        target: &ProviderCalendarTarget,
        master_provider_event_id: &str,
        original_start: &str,
    ) -> Result<ProviderSeriesMutationOutcome, CalendarProviderError> {
        self.for_link(target.email_link_id)
            .await?
            .truncate_recurring_event(
                access_token,
                target,
                master_provider_event_id,
                original_start,
            )
            .await
    }

    async fn rsvp_event(
        &self,
        access_token: &str,
        target: &ProviderCalendarTarget,
        master_provider_event_id: &str,
        actor: &ActorInboxes,
        response: AttendeeResponseStatus,
        scope: &CalendarRsvpScope,
    ) -> Result<ProviderRsvpOutcome, CalendarProviderError> {
        self.for_link(target.email_link_id)
            .await?
            .rsvp_event(
                access_token,
                target,
                master_provider_event_id,
                actor,
                response,
                scope,
            )
            .await
    }

    async fn stop_watch_channel(
        &self,
        access_token: &str,
        email_link_id: Uuid,
        channel_id: &str,
        resource_id: &str,
    ) -> Result<(), CalendarProviderError> {
        self.for_link(email_link_id)
            .await?
            .stop_watch_channel(access_token, email_link_id, channel_id, resource_id)
            .await
    }
}

#[cfg(all(test, feature = "inbound"))]
mod test {
    use super::*;
    use crate::outbound::google::UnmeteredGate;
    use axum::{Router, extract::Request, http::StatusCode, routing::any};
    use macro_db_migrator::MACRO_DB_MIGRATIONS;
    use tokio::sync::mpsc;

    async fn endpoint(
        label: &'static str,
        tx: mpsc::Sender<&'static str>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().fallback(any(move |request: Request| {
            let tx = tx.clone();
            async move {
                assert_eq!(request.uri().path(), "/calendar/v3/channels/stop");
                tx.send(label).await.unwrap();
                StatusCode::OK
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/calendar/v3", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, task)
    }

    #[sqlx::test(migrator = "MACRO_DB_MIGRATIONS")]
    async fn routes_calendar_calls_by_persisted_link_provider(pool: PgPool) {
        let (tx, mut rx) = mpsc::channel(2);
        let (google_base, google_task) = endpoint("google", tx.clone()).await;
        let (stalwart_base, stalwart_task) = endpoint("stalwart", tx).await;
        let google_link = Uuid::now_v7();
        let stalwart_link = Uuid::now_v7();
        for id in [google_link, stalwart_link] {
            sqlx::query("INSERT INTO email_links (id, macro_id, fusionauth_user_id, email_address, provider) VALUES ($1, $2, $2, $3, 'GMAIL')")
                .bind(id)
                .bind(format!("macro|{id}@example.com"))
                .bind(format!("{id}@example.com"))
                .execute(&pool).await.unwrap();
        }
        sqlx::query("UPDATE email_links SET provider = 'STALWART' WHERE id = $1")
            .bind(stalwart_link)
            .execute(&pool)
            .await
            .unwrap();
        let client = RoutedCalendarClient::new(
            pool,
            GoogleCalendarClient::with_gate_at_base(
                reqwest::Client::new(),
                UnmeteredGate,
                google_base,
            ),
            Some(GoogleCalendarClient::with_gate_at_base(
                reqwest::Client::new(),
                UnmeteredGate,
                stalwart_base,
            )),
        );
        for (id, expected) in [(google_link, "google"), (stalwart_link, "stalwart")] {
            client
                .stop_watch_channel("token", id, "channel", "resource")
                .await
                .unwrap();
            assert_eq!(rx.recv().await, Some(expected));
        }
        google_task.abort();
        stalwart_task.abort();
    }
}
