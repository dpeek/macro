//! Stalwart mailbox provisioning from the FusionAuth sign-in grant.
use crate::api::{ApiContext, context::AuthorizationService};
use crate::stalwart_push::StalwartPushVerifier;
use authentication_service_client::error::AuthServiceClientError;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use calendar_events::domain::models::{CalendarGrantIntent, GoogleScopeSet};
use chrono::Utc;
use macro_authorization::{MacroAuthorizationExtractor, UserOrInternal};
use macro_user_id::{email::EmailStr, user_id::MacroUserIdStr};
use models_email::email::service::backfill::{
    BackfillOperation, BackfillPubsubMessage, InitPayload, JobScopedPayload,
};
use models_email::gmail::inbox_sync::{
    GmailInboxSyncPayload, GmailMessagePayload, InboxSyncOperation, InboxSyncPubsubMessage,
};
use models_email::service::link::{Link, UserProvider};
use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize)]
pub struct AutomaticStartResponse {
    authorization_url: Option<String>,
}

/// Accepts only Stalwart-signed mailbox hints and queues their history cursor.
pub async fn webhook(
    State(ctx): State<ApiContext>,
    headers: HeaderMap,
    Json(req): Json<GmailInboxSyncPayload>,
) -> Result<Response, Response> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| failure(StatusCode::UNAUTHORIZED, "Missing Fog push bearer token"))?;
    let verifier = StalwartPushVerifier::from_env()
        .map_err(|error| {
            tracing::error!(error=?error, "Fog push configuration is invalid");
            failure(StatusCode::SERVICE_UNAVAILABLE, "Fog push is unavailable")
        })?
        .ok_or_else(|| failure(StatusCode::SERVICE_UNAVAILABLE, "Fog push is unavailable"))?;
    let jwks = verifier.fetch_keys().await.map_err(|error| {
        tracing::warn!(error=?error, "Unable to fetch Fog signing keys");
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Fog signing keys are unavailable",
        )
    })?;
    verifier
        .verify(bearer, &req.subscription, &jwks)
        .map_err(|error| {
            tracing::warn!(error=?error, "Invalid Fog push token");
            failure(StatusCode::UNAUTHORIZED, "Invalid Fog push token")
        })?;

    let link_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM email_links \
         WHERE LOWER(email_address) = LOWER($1) \
         AND provider = 'GMAIL' AND is_sync_active = TRUE \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&req.message.data.email_address)
    .fetch_optional(&ctx.db)
    .await
    .map_err(|error| {
        tracing::error!(error=?error, "Unable to find pushed mailbox");
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to queue mailbox hint",
        )
    })?;
    if let Some(link_id) = link_id {
        ctx.sqs_client
            .enqueue_gmail_inbox_sync_notification(InboxSyncPubsubMessage {
                link_id,
                operation: InboxSyncOperation::GmailMessage(GmailMessagePayload {
                    history_id: req.message.data.history_id,
                }),
            })
            .await
            .map_err(|error| {
                tracing::error!(error=?error, "Unable to queue mailbox hint");
                failure(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Unable to queue mailbox hint",
                )
            })?;
    }
    Ok(StatusCode::ACCEPTED.into_response())
}

fn failure(status: StatusCode, message: &'static str) -> Response {
    (status, message).into_response()
}

/// Provision an eligible user's own mailbox after Stalwart SSO. The auth
/// service validates the FusionAuth grant, verified identity, and mail scope.
pub async fn start_automatic(
    State(ctx): State<ApiContext>,
    authorization: MacroAuthorizationExtractor<AuthorizationService, UserOrInternal>,
) -> Result<Json<AutomaticStartResponse>, Response> {
    let user = &authorization.authorization.user;
    let macro_id = user.macro_user_id.as_ref();
    let email = macro_id
        .strip_prefix("macro|")
        .ok_or_else(|| failure(StatusCode::INTERNAL_SERVER_ERROR, "Invalid Macro account"))?;
    let existing = email_db_client::links::get::fetch_links_by_fusionauth_user_id(
        &ctx.db,
        &user.user_context.fusion_user_id,
    )
    .await
    .map_err(|error| {
        tracing::error!(error=?error, "Unable to inspect mailbox links");
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to inspect mailbox links",
        )
    })?;
    let existing_link = existing.iter().find(|link| {
        link.provider == UserProvider::Gmail
            && link.email_address.0.as_ref().eq_ignore_ascii_case(email)
            && link.is_sync_active
    });
    let grant = match ctx
        .auth_service_client
        .get_stalwart_access_token(&user.user_context.fusion_user_id, email)
        .await
    {
        Ok(grant) => grant,
        Err(AuthServiceClientError::NotFound | AuthServiceClientError::Forbidden) => {
            return Ok(Json(AutomaticStartResponse {
                authorization_url: None,
            }));
        }
        Err(error) => {
            tracing::warn!(error=?error, "Stalwart SSO grant cannot provision mailbox");
            return Err(failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "Stalwart mailbox is unavailable",
            ));
        }
    };
    if let Some(link) = existing_link {
        apply_calendar_scope(&ctx, link.id, grant.scope.as_deref().unwrap_or_default())
            .await
            .map_err(|error| {
                tracing::warn!(error=?error, link_id=%link.id, "Unable to record Stalwart calendar grant");
                failure(StatusCode::SERVICE_UNAVAILABLE, "Calendar grant is unavailable")
            })?;
        return Ok(Json(AutomaticStartResponse {
            authorization_url: None,
        }));
    }

    let macro_id = MacroUserIdStr::try_from(macro_id.to_string())
        .map_err(|_| failure(StatusCode::INTERNAL_SERVER_ERROR, "Invalid Macro account"))?;
    let email_address = EmailStr::try_from(email.to_string())
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "Invalid mailbox address"))?;
    let now = Utc::now();
    let link = Link {
        id: Uuid::new_v4(),
        is_primary: Link::derive_is_primary(&macro_id, &email_address),
        macro_id,
        fusionauth_user_id: user.user_context.fusion_user_id.clone(),
        email_address,
        provider: UserProvider::Gmail,
        is_sync_active: false,
        needs_reauth: false,
        last_sync_error_at: None,
        created_at: now,
        updated_at: now,
    };
    let mut tx = ctx.db.begin().await.map_err(|_| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to save mailbox link",
        )
    })?;
    let link = email_db_client::links::insert::upsert_link(&mut tx, link)
        .await
        .map_err(|error| {
            tracing::error!(error=?error, "Unable to save mailbox link");
            failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to save mailbox link",
            )
        })?;
    tx.commit().await.map_err(|_| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to save mailbox link",
        )
    })?;
    activate_link(&ctx, &link).await.map_err(|error| {
        tracing::error!(error=?error, link_id=%link.id, "Unable to activate mailbox");
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to activate mailbox",
        )
    })?;
    apply_calendar_scope(&ctx, link.id, grant.scope.as_deref().unwrap_or_default())
        .await
        .map_err(|error| {
            tracing::warn!(error=?error, link_id=%link.id, "Unable to record Stalwart calendar grant");
            failure(StatusCode::SERVICE_UNAVAILABLE, "Calendar grant is unavailable")
        })?;
    Ok(Json(AutomaticStartResponse {
        authorization_url: None,
    }))
}

/// Record only scopes returned by the verified Stalwart refresh grant.
pub(crate) async fn apply_calendar_scope(
    ctx: &ApiContext,
    link_id: Uuid,
    scope: &str,
) -> anyhow::Result<()> {
    ctx.calendar_service
        .apply_google_grant(
            link_id,
            GoogleScopeSet::parse(scope),
            CalendarGrantIntent::Incidental,
        )
        .await?;
    Ok(())
}

async fn activate_link(ctx: &ApiContext, link: &Link) -> anyhow::Result<()> {
    let subscription = ctx
        .email_api
        .register_subscription_without_cache(link)
        .await?;
    let mut tx = ctx.db.begin().await?;
    email_db_client::histories::upsert_gmail_history(
        tx.as_mut(),
        link.id,
        subscription.cursor.as_str(),
    )
    .await?;
    sqlx::query("UPDATE email_links SET is_sync_active = TRUE, updated_at = NOW() WHERE id = $1")
        .bind(link.id)
        .execute(tx.as_mut())
        .await?;
    tx.commit().await?;

    if let Some(job) = email_db_client::backfill::job::insert::create_backfill_job(
        &ctx.db,
        link.id,
        &link.fusionauth_user_id,
        None,
        false,
    )
    .await?
    {
        let message = BackfillPubsubMessage {
            backfill_operation: BackfillOperation::Init(JobScopedPayload {
                link_id: link.id,
                job_id: job.id,
                payload: InitPayload {},
            }),
        };
        if let Err(error) = ctx.sqs_client.enqueue_email_backfill_message(message).await {
            email_db_client::backfill::job::update::fail_backfill_job(&ctx.db, job.id).await?;
            return Err(error.into());
        }
    }
    Ok(())
}
