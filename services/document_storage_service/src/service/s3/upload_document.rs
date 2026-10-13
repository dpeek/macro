use aws_sdk_s3::{self as s3, primitives::ByteStream};

#[tracing::instrument(skip(client))]
pub(in crate::service::s3) async fn upload_document(
    client: &s3::Client,
    bucket: &str,
    key: &str,
    byte_stream: ByteStream,
) -> anyhow::Result<()> {
    if cfg!(feature = "local") && !macro_aws_config::has_s3_endpoint_override() {
        return Ok(());
    }

    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(byte_stream)
        .send()
        .await?;

    Ok(())
}
