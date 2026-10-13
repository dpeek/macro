//! Exercise the agent harness's two patch adapters against disposable RustFS.

use agent_changes::domain::ports::{ChangesetBlobStore, PatchBlobKey};
use agent_changes::outbound::s3::S3ChangesetBlobStore;
use github_pull_requests::domain::ports::GithubPullRequestPatchStore;
use github_pull_requests::outbound::s3_patch_store::S3GithubPullRequestPatchStore;

#[tokio::test]
#[ignore = "requires disposable RustFS; use run-direct-clients.sh"]
async fn agent_and_github_patch_adapters_use_rustfs() -> anyhow::Result<()> {
    let endpoint = std::env::var("FOG_DIRECT_S3_PROXY_URL")?;
    assert_eq!(std::env::var("AWS_ENDPOINT_URL_S3")?, endpoint);
    let s3 = macro_aws_config::s3_client().await;
    let agent_bucket = "agent-session-changes";
    let github_bucket = "github-pull-request-patches";
    for bucket in [agent_bucket, github_bucket] {
        s3.create_bucket().bucket(bucket).send().await?;
    }

    let agent = S3ChangesetBlobStore::new(s3.clone(), agent_bucket.to_owned());
    let agent_key = PatchBlobKey::from_stored(
        "agent-sessions/synthetic-session/changes/synthetic-capture.patch".to_owned(),
    );
    assert!(agent.get_patch(&agent_key).await?.is_none());
    let patch = "diff --git a/old b/new\n+synthetic agent patch\n";
    agent.put_patch(&agent_key, patch).await?;
    assert_eq!(agent.get_patch(&agent_key).await?.as_deref(), Some(patch));
    assert_eq!(
        s3.head_object()
            .bucket(agent_bucket)
            .key(agent_key.as_str())
            .send()
            .await?
            .content_type(),
        Some("text/x-diff; charset=utf-8")
    );
    agent.delete_patch(&agent_key).await?;
    assert!(agent.get_patch(&agent_key).await?.is_none());
    agent.delete_patch(&agent_key).await?;

    let github = S3GithubPullRequestPatchStore::new(s3.clone(), github_bucket.to_owned());
    let github_key = "pull-requests/synthetic-repo/1/base-head.patch";
    assert!(github.get_patch(github_key).await?.is_none());
    github.put_patch(github_key, "first patch").await?;
    assert_eq!(
        github.get_patch(github_key).await?.as_deref(),
        Some("first patch")
    );
    github.put_patch(github_key, "replacement patch").await?;
    assert_eq!(
        github.get_patch(github_key).await?.as_deref(),
        Some("replacement patch")
    );
    assert_eq!(
        s3.head_object()
            .bucket(github_bucket)
            .key(github_key)
            .send()
            .await?
            .content_type(),
        Some("text/x-diff; charset=utf-8")
    );
    Ok(())
}
