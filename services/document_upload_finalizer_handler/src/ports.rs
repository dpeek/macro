use std::future::Future;

use documents::domain::models::DocumentError;
use model::document::DocumentBasic;

/// Document metadata lookup needed by the upload finalization use case.
pub trait DocumentUploadMetadataPort: Send + Sync {
    /// Fetch the basic document context for a document id.
    fn get_basic_document(
        &self,
        document_id: &str,
    ) -> impl Future<Output = Result<Option<DocumentBasic>, DocumentError>> + Send;
}

/// Object storage read operations needed by the upload finalization use case.
pub trait DocumentObjectReader: Send + Sync {
    /// Distinguish a late event for a removed object from a still-pending upload.
    fn object_exists(
        &self,
        bucket: &str,
        key: &str,
    ) -> impl Future<Output = Result<bool, anyhow::Error>> + Send;

    /// Read an object as UTF-8 text.
    fn read_utf8_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> impl Future<Output = Result<String, anyhow::Error>> + Send;
}
