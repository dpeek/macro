use super::*;

#[test]
fn stalwart_provider_has_a_distinct_api_value() {
    let provider = UserProvider::from(crate::email::service::link::UserProvider::Stalwart);
    assert_eq!(provider.as_str(), "STALWART");
    assert_eq!(serde_json::to_string(&provider).unwrap(), "\"STALWART\"");
}

#[test]
fn inactive_link_is_inactive_regardless_of_reauth() {
    assert_eq!(SyncStatus::derive(false, false, None), SyncStatus::Inactive);
    assert_eq!(SyncStatus::derive(false, true, None), SyncStatus::Inactive);
}

#[test]
fn needs_reauth_takes_precedence_over_backfill_state() {
    assert_eq!(
        SyncStatus::derive(true, true, None),
        SyncStatus::NeedsReauth
    );
    assert_eq!(
        SyncStatus::derive(true, true, Some(BackfillJobStatus::InProgress)),
        SyncStatus::NeedsReauth
    );
    assert_eq!(
        SyncStatus::derive(true, true, Some(BackfillJobStatus::Complete)),
        SyncStatus::NeedsReauth
    );
}

#[test]
fn healthy_link_derives_from_backfill_state() {
    assert_eq!(SyncStatus::derive(true, false, None), SyncStatus::UpToDate);
    assert_eq!(
        SyncStatus::derive(true, false, Some(BackfillJobStatus::Complete)),
        SyncStatus::UpToDate
    );
    assert_eq!(
        SyncStatus::derive(true, false, Some(BackfillJobStatus::Init)),
        SyncStatus::Syncing
    );
    assert_eq!(
        SyncStatus::derive(true, false, Some(BackfillJobStatus::InProgress)),
        SyncStatus::Syncing
    );
    assert_eq!(
        SyncStatus::derive(true, false, Some(BackfillJobStatus::Failed)),
        SyncStatus::Error
    );
    assert_eq!(
        SyncStatus::derive(true, false, Some(BackfillJobStatus::Cancelled)),
        SyncStatus::Error
    );
}
