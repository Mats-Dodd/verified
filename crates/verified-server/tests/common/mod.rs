//! What the server's integration tests share.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::client::legacy::connect::HttpConnector;
use verified_server::limits::ServerLimits;
use verified_server::model::anthropic::{AnthropicConfig, AnthropicModel, ApiKey};
use verified_server::model::fake;
use verified_server::seam::{Clock, Tasks};

/// `work`, or an error once `limit` passes: a test's own bound on how long
/// it waits for the code under test, never a seam of that code.
pub(crate) async fn within<F: Future>(
    limit: Duration,
    work: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    tokio::time::timeout(limit, work).await
}

/// A listener on a free loopback port, bound directly (not through the
/// shim, which is turmoil's under `sim`).
pub(crate) async fn listener() -> (tokio::net::TcpListener, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port");
    let addr = listener.local_addr().expect("bound");
    (listener, addr)
}

/// The real Anthropic client, talking to a model host at `addr`.
pub(crate) fn client(
    addr: SocketAddr,
    limits: &ServerLimits,
    clock: Arc<dyn Clock>,
) -> AnthropicModel<HttpConnector> {
    AnthropicModel::new(
        HttpConnector::new(),
        AnthropicConfig {
            base: format!("http://{addr}").parse().expect("a uri"),
            api_key: ApiKey::new("test-key".to_owned()),
            model: "claude-test".to_owned(),
        },
        limits,
        clock,
    )
    .expect("a client")
}

/// The scripted Anthropic ([`fake::router`]) served on loopback by `tasks`,
/// and the real client talking to it.
pub(crate) async fn fake_model(
    limits: &ServerLimits,
    clock: &Arc<dyn Clock>,
    tasks: &Tasks,
) -> AnthropicModel<HttpConnector> {
    let (listener, addr) = listener().await;
    let host = fake::router(Arc::clone(clock), tasks.clone());
    tasks.spawn(async move {
        drop(axum::serve(listener, host).await);
    });
    client(addr, limits, Arc::clone(clock))
}
