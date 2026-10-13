//! Outbound calendar adapters.

/// Calendar-service HTTP adapter for the mutation port.
#[cfg(feature = "mutation-client")]
pub mod calendar_service_mutations;
/// Meeting-link adapter over the call domain service.
#[cfg(feature = "call-meetings")]
pub mod call_meeting_links;
/// Google Calendar API adapter.
#[cfg(feature = "google")]
pub mod google;
/// Notification-service calendar reminder notifier.
#[cfg(feature = "notify")]
pub mod notification_notifier;
/// PostgreSQL calendar repository.
#[cfg(feature = "postgres")]
pub mod pg;
/// PostgreSQL facts for team-calendar sharing.
#[cfg(feature = "postgres")]
pub mod pg_team;
/// Select the calendar HTTP backend from the linked account.
#[cfg(all(feature = "google", feature = "postgres"))]
pub mod routed;
/// SQS calendar reminder dispatch queue.
#[cfg(feature = "dispatch-sqs")]
pub mod sqs_dispatch_queue;
