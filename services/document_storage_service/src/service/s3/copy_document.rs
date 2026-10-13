use aws_sdk_s3 as s3;
use tracing::instrument;

#[instrument(skip(client))]
pub(in crate::service::s3) async fn copy_document(
    client: &s3::Client,
    bucket: &str,
    source_key: &str,
    destination_key: &str,
) -> anyhow::Result<()> {
    if cfg!(feature = "local") && !macro_aws_config::has_s3_endpoint_override() {
        return Ok(());
    }

    client
        .copy_object()
        .bucket(bucket)
        .copy_source(format!("{}/{}", bucket, source_key))
        .key(destination_key)
        .send()
        .await?;

    Ok(())
}
