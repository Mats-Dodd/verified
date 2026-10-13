//! What the server's integration tests share.

use std::time::Duration;

/// `work`, or an error once `limit` passes: a test's own bound on how long
/// it waits for the code under test, never a seam of that code.
pub(crate) async fn within<F: Future>(
    limit: Duration,
    work: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    tokio::time::timeout(limit, work).await
}
