//! Calendar backfill queue consumer.
//!
//! Drains calendar's own backfill queue, resolves the connected inbox's grant,
//! and drives the Google Calendar backfill coordinator. Modeled on the calendar
//! branch of email_service's backfill worker, but self-contained: tokens come
//! from the selected provider's existing authentication path.

use std::sync::Arc;
use std::time::Duration;

use authentication_service_client::{AuthServiceClient, error::AuthServiceClientError};
use calendar_events::domain::{
    models::{
        CalendarBackfillFailureDisposition, CalendarBackfillJobKey, CalendarProvider,
        GoogleBackfillRunReport, GoogleScopeSet, GoogleWatchConfig, OccurrenceRange,
    },
    service::{
        CalendarReauthAnnouncer, GoogleCalendarBackfillCoordinator,
        GoogleCalendarBackfillFailureService, GoogleCalendarBackfillRunError,
    },
};
use calendar_events::outbound::{google::GoogleCalendarClient, pg::PgCalendarRepository};
use chrono::Utc;
use macro_event_broker::{KafkaEventPublisher, MacroEventBrokerService};
use models_email::email::service::link::Link;
use redis::aio::MultiplexedConnection;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::backfill_queue::CalendarBackfillMessage;
use crate::calendar_backfill_adapters::RedisCalendarRequestGate;
use crate::calendar_outbox::republish_calendar_job;
use crate::calendar_ratelimit::CalendarRateLimiter;
use crate::calendar_reauth::LinkManagerReauthNotifier;
use crate::pubsub_util::cg_refresh_calendar;

/// The event broker used by the backfill coordinator.
pub type CalendarEventBroker = MacroEventBrokerService<KafkaEventPublisher, TaskTracker>;

/// Concrete Google Calendar backfill coordinator for this service.
type GoogleCalendarBackfillService = GoogleCalendarBackfillCoordinator<
    PgCalendarRepository,
    GoogleCalendarClient<RedisCalendarRequestGate>,
    PgCalendarRepository,
    CalendarEventBroker,
>;

/// Concrete pre-lease Google Calendar failure handler for this service.
type GoogleCalendarBackfillFailureHandler =
    GoogleCalendarBackfillFailureService<PgCalendarRepository>;

/// Concrete reauth-required announcer for this service.
type CalendarReauth = CalendarReauthAnnouncer<LinkManagerReauthNotifier>;

/// Everything a backfill worker needs to process one calendar delivery.
#[derive(Clone)]
pub struct CalendarBackfillContext {
    db: PgPool,
    sqs_worker: sqs_worker::SQSWorker,
    redis_conn: MultiplexedConnection,
    auth_service_client: AuthServiceClient,
    connection_gateway_client: connection_gateway_client::client::ConnectionGatewayClient,
    google_coordinator: Arc<GoogleCalendarBackfillService>,
    stalwart_coordinator: Option<Arc<GoogleCalendarBackfillService>>,
    google_failure: Arc<GoogleCalendarBackfillFailureHandler>,
    stalwart_failure: Option<Arc<GoogleCalendarBackfillFailureHandler>>,
    reauth: Arc<CalendarReauth>,
    calendar_sync_enabled: bool,
    team_sharing_enabled: bool,
}

impl CalendarBackfillContext {
    /// Compose the backfill context from process-level adapters.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: PgPool,
        sqs_worker: sqs_worker::SQSWorker,
        rate_limiter: CalendarRateLimiter,
        redis_conn: MultiplexedConnection,
        auth_service_client: AuthServiceClient,
        connection_gateway_client: connection_gateway_client::client::ConnectionGatewayClient,
        macro_event_broker: CalendarEventBroker,
        reauth_notifier: LinkManagerReauthNotifier,
        watch: Option<GoogleWatchConfig>,
        calendar_sync_enabled: bool,
        team_sharing_enabled: bool,
        stalwart_api_base: Option<&str>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("calendar client configuration is valid");
        let google_repository =
            PgCalendarRepository::with_provider(db.clone(), CalendarProvider::Google);
        let google_coordinator = Arc::new(GoogleCalendarBackfillCoordinator::new(
            google_repository.clone(),
            GoogleCalendarClient::with_gate(
                http.clone(),
                RedisCalendarRequestGate::new(rate_limiter.clone()),
            ),
            google_repository.clone(),
            macro_event_broker.clone(),
            watch,
        ));
        let google_failure = Arc::new(GoogleCalendarBackfillFailureService::new(google_repository));
        let (stalwart_coordinator, stalwart_failure) = if let Some(api_base) = stalwart_api_base {
            let repository =
                PgCalendarRepository::with_provider(db.clone(), CalendarProvider::Stalwart);
            (
                Some(Arc::new(GoogleCalendarBackfillCoordinator::new(
                    repository.clone(),
                    GoogleCalendarClient::with_gate_at_base(
                        http,
                        RedisCalendarRequestGate::new(rate_limiter),
                        api_base,
                    ),
                    repository.clone(),
                    macro_event_broker,
                    None,
                ))),
                Some(Arc::new(GoogleCalendarBackfillFailureService::new(
                    repository,
                ))),
            )
        } else {
            (None, None)
        };
        let reauth = Arc::new(CalendarReauthAnnouncer::new(reauth_notifier));
        Self {
            db,
            sqs_worker,
            redis_conn,
            auth_service_client,
            connection_gateway_client,
            google_coordinator,
            stalwart_coordinator,
            google_failure,
            stalwart_failure,
            reauth,
            calendar_sync_enabled,
            team_sharing_enabled,
        }
    }
}

/// The queue-visible outcome of processing a delivery.
enum Disposition {
    /// Delete the message; work completed or reached a terminal state.
    Ack,
    /// Leave the message for redelivery after its visibility timeout.
    Retry(anyhow::Error),
}

/// Poll the calendar backfill queue until cancellation is requested, restarting
/// the inner loop if it panics.
pub async fn run_worker(
    ctx: CalendarBackfillContext,
    worker: sqs_worker::SQSWorker,
    cancellation_token: CancellationToken,
) {
    loop {
        let worker_result = tokio::spawn({
            let ctx = ctx.clone();
            let worker = worker.clone();
            let cancellation_token = cancellation_token.clone();
            async move {
                loop {
                    let receive_result = tokio::select! {
                        result = worker.receive_messages() => result,
                        _ = cancellation_token.cancelled() => return,
                    };
                    match receive_result {
                        Ok(messages) => {
                            for message in &messages {
                                if let Err(error) = process_message(&ctx, message).await {
                                    tracing::error!(
                                        message_id = message.message_id,
                                        error = ?error,
                                        "error processing calendar backfill message"
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            tracing::error!(error = ?error, "error receiving calendar backfill messages");
                        }
                    }
                }
            }
        })
        .await;

        if cancellation_token.is_cancelled() {
            return;
        }

        match worker_result {
            Ok(()) => tracing::error!("calendar backfill worker exited unexpectedly"),
            Err(error) => tracing::error!(error = ?error, "calendar backfill worker crashed"),
        }

        tracing::info!("CALENDAR BACKFILL WORKER RESTARTING...");
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            _ = cancellation_token.cancelled() => return,
        }
    }
}

async fn process_message(
    ctx: &CalendarBackfillContext,
    message: &aws_sdk_sqs::types::Message,
) -> anyhow::Result<()> {
    // Malformed JSON is not retryable: ack it so it does not redeliver forever.
    let data = match extract_message(message) {
        Ok(data) => data,
        Err(error) => {
            tracing::error!(error = %error, "failed to extract calendar backfill message; acking");
            sqs_worker::cleanup_message(&ctx.sqs_worker, message)
                .await
                .inspect_err(|cleanup_error| {
                    tracing::error!(error = %cleanup_error, "failed to clean up unparseable message");
                })
                .ok();
            return Err(error);
        }
    };

    match dispatch(ctx, &data).await {
        Disposition::Ack => {
            sqs_worker::cleanup_message(&ctx.sqs_worker, message).await?;
            Ok(())
        }
        Disposition::Retry(error) => {
            tracing::warn!(error = ?error, "retryable calendar backfill error; leaving message for redelivery");
            Ok(())
        }
    }
}

async fn dispatch(ctx: &CalendarBackfillContext, data: &CalendarBackfillMessage) -> Disposition {
    match data {
        CalendarBackfillMessage::GoogleCalendar {
            link_id,
            calendar_job_id,
        } => google_backfill(ctx, *link_id, *calendar_job_id).await,
    }
}

#[tracing::instrument(skip(ctx), fields(%calendar_job_id, %link_id))]
async fn google_backfill(
    ctx: &CalendarBackfillContext,
    link_id: Uuid,
    calendar_job_id: Uuid,
) -> Disposition {
    // When calendar sync is disabled, return the delivery to the outbox and ack;
    // the drain republishes it once the switch flips back on.
    if !ctx.calendar_sync_enabled {
        tracing::info!(%calendar_job_id, "calendar sync disabled; returning delivery to the outbox");
        return match republish_calendar_job(&ctx.db, calendar_job_id).await {
            Ok(()) => Disposition::Ack,
            Err(error) => Disposition::Retry(
                error.context("failed to return calendar delivery to the outbox"),
            ),
        };
    }

    let link = match email_db_client::links::get::fetch_link_by_id(&ctx.db, link_id).await {
        Ok(Some(link)) => link,
        Ok(None) => {
            tracing::error!(%link_id, "link not found for calendar backfill; failing job");
            return fail_unclaimed(
                ctx,
                link_id,
                calendar_job_id,
                CalendarBackfillFailureDisposition::Permanent,
                "link not found for calendar backfill",
                CalendarProvider::Google,
            )
            .await;
        }
        Err(error) => {
            return Disposition::Retry(error.context("failed to load link for calendar backfill"));
        }
    };

    // Calendar jobs can be created immediately after an incremental Google scope
    // grant. Bypass the token cache so this job does not reuse a pre-consent
    // access token that lacks calendar scopes.
    let key = email_utils::token_cache_key::TokenCacheKey::new(
        &link.fusionauth_user_id,
        link.email_address.0.as_ref(),
        link.provider.as_str(),
    );
    let Some(provider) = CalendarProvider::from_link_provider(link.provider.as_str()) else {
        return fail_unclaimed(
            ctx,
            link_id,
            calendar_job_id,
            CalendarBackfillFailureDisposition::Permanent,
            "unsupported calendar link provider",
            CalendarProvider::Google,
        )
        .await;
    };
    let token_result = match provider {
        CalendarProvider::Google => {
            google_token::fetch_gmail_access_token_no_cache(
                &key,
                &ctx.redis_conn,
                &ctx.auth_service_client,
            )
            .await
        }
        CalendarProvider::Stalwart => ctx
            .auth_service_client
            .get_stalwart_access_token(&link.fusionauth_user_id, link.email_address.0.as_ref())
            .await
            .map_err(anyhow::Error::from)
            .and_then(|token| {
                let scopes = GoogleScopeSet::parse(token.scope.as_deref().unwrap_or_default());
                if scopes.has_calendar_capability_for(CalendarProvider::Stalwart) {
                    Ok(token.access_token)
                } else {
                    Err(AuthServiceClientError::Forbidden.into())
                }
            }),
        CalendarProvider::Outlook => {
            return fail_unclaimed(
                ctx,
                link_id,
                calendar_job_id,
                CalendarBackfillFailureDisposition::Permanent,
                "Outlook links do not use the Google-shaped calendar backfill queue",
                provider,
            )
            .await;
        }
    };
    let access_token = match token_result {
        Ok(token) => token,
        Err(error) => {
            if is_reauth_required_error(&error) {
                return fail_unclaimed(
                    ctx,
                    link_id,
                    calendar_job_id,
                    CalendarBackfillFailureDisposition::ReauthRequired,
                    &format!("{error:?}"),
                    provider,
                )
                .await;
            }
            return Disposition::Retry(
                error.context("failed to fetch token for Google Calendar backfill"),
            );
        }
    };

    run_coordinator(ctx, &access_token, &link, calendar_job_id, provider).await
}

async fn run_coordinator(
    ctx: &CalendarBackfillContext,
    access_token: &str,
    link: &Link,
    calendar_job_id: Uuid,
    provider: CalendarProvider,
) -> Disposition {
    let mut report = GoogleBackfillRunReport::default();
    let coordinator = match provider {
        CalendarProvider::Google => &ctx.google_coordinator,
        CalendarProvider::Stalwart => match &ctx.stalwart_coordinator {
            Some(coordinator) => coordinator,
            None => {
                return Disposition::Retry(anyhow::anyhow!(
                    "Stalwart calendar API is not configured"
                ));
            }
        },
        CalendarProvider::Outlook => {
            return Disposition::Retry(anyhow::anyhow!(
                "Outlook links do not use the Google-shaped calendar backfill coordinator"
            ));
        }
    };
    let run_result = coordinator
        .run(
            CalendarBackfillJobKey {
                job_id: calendar_job_id,
                email_link_id: link.id,
            },
            link.macro_id.as_ref(),
            access_token,
            OccurrenceRange::maintenance_horizon(Utc::now()),
            &mut report,
        )
        .await;
    // Quiet token-only polls change nothing; only real changes nudge active
    // viewers to refetch. The nudge goes out even when the run ultimately
    // failed: per-calendar commits before the error are durable, and the
    // retry's quiet re-run would never report them.
    if report.changed() {
        cg_refresh_calendar(
            &ctx.connection_gateway_client,
            &ctx.db,
            link.macro_id.as_ref(),
            link.id,
            ctx.team_sharing_enabled,
            report.team_ooo_changed || run_result.is_err(),
        )
        .await;
    }

    match run_result {
        Ok(()) => Disposition::Ack,
        Err(error) => {
            ctx.reauth.announce_run_error(link.id, &error).await;
            let retryable = matches!(
                error,
                GoogleCalendarBackfillRunError::Busy
                    | GoogleCalendarBackfillRunError::LeaseLost
                    | GoogleCalendarBackfillRunError::Retryable(_)
            );
            if retryable {
                Disposition::Retry(anyhow::Error::new(error))
            } else {
                // The coordinator already recorded this post-lease failure; ack.
                tracing::error!(error = ?error, %calendar_job_id, "non-retryable Google Calendar backfill failure");
                Disposition::Ack
            }
        }
    }
}

/// Terminate an unclaimed job whose token fetch or link lookup failed before a
/// lease could be claimed. Returns [`Disposition::Retry`] when the terminal
/// state cannot be persisted, so the delivery redelivers rather than acking a
/// job that was left non-terminal.
async fn fail_unclaimed(
    ctx: &CalendarBackfillContext,
    link_id: Uuid,
    calendar_job_id: Uuid,
    disposition: CalendarBackfillFailureDisposition,
    message: &str,
    provider: CalendarProvider,
) -> Disposition {
    let failure = match provider {
        CalendarProvider::Google => &ctx.google_failure,
        CalendarProvider::Stalwart => ctx.stalwart_failure.as_ref().unwrap_or(&ctx.google_failure),
        CalendarProvider::Outlook => &ctx.google_failure,
    };
    match failure
        .fail_unclaimed(
            CalendarBackfillJobKey {
                job_id: calendar_job_id,
                email_link_id: link_id,
            },
            disposition,
            message,
        )
        .await
    {
        Ok(outcome) => {
            ctx.reauth.announce_unclaimed(link_id, &outcome).await;
            Disposition::Ack
        }
        Err(error) => {
            tracing::error!(error = ?error, %calendar_job_id, "failed to record terminal calendar backfill failure");
            Disposition::Retry(anyhow::anyhow!(
                "failed to persist terminal calendar backfill failure: {error:?}"
            ))
        }
    }
}

fn is_reauth_required_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<AuthServiceClientError>()
            .is_some_and(|cause| {
                matches!(
                    cause,
                    AuthServiceClientError::Forbidden | AuthServiceClientError::NotFound
                )
            })
    })
}

fn extract_message(
    message: &aws_sdk_sqs::types::Message,
) -> anyhow::Result<CalendarBackfillMessage> {
    let body = message
        .body()
        .ok_or_else(|| anyhow::anyhow!("message body not found"))?;
    serde_json::from_str(body).map_err(|error| {
        anyhow::anyhow!("failed to deserialize calendar backfill message: {error}")
    })
}
