//! The simulated hosts: the daemon and the fake Anthropic.
//!
//! The daemon is the real one (`verified_server` with its `sim` shim) on
//! turmoil's network and filesystem, with a seeded Entropy, the simulated
//! Clock, the fake shell of [`SimEnv`] and a log store that reports what is
//! durable. It talks to the fake Anthropic over the simulated network with
//! the real Anthropic client.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use verified_domain::id::UnixMillis;
use verified_server::daemon::{self, Layout};
use verified_server::env::LocalEnv;
use verified_server::limits::ServerLimits;
use verified_server::model::anthropic::{AnthropicConfig, AnthropicModel, ApiKey};
use verified_server::model::fake;
use verified_server::seam::{Clock, NoLock, World};
use verified_server::shell::{Planted, Seams, ShellConfig};
use verified_server::shim::time::SimClock;
use verified_server::shim::{fs, net};

use crate::entropy::DaemonEntropy;
use crate::env::SimEnv;
use crate::ledger::{Entry, Ledger};
use crate::scenario::FILES;
use crate::store::WatchedStore;

/// The daemon's host name.
pub const DAEMON: &str = "daemon";
/// The daemon's port.
pub const PORT: u16 = 7777;
/// The fake Anthropic's host name.
pub const ANTHROPIC: &str = "anthropic";
/// The fake Anthropic's port.
pub const MODEL_PORT: u16 = 4000;
/// The TUI's host name.
pub const TUI: &str = "tui";
/// The model name the daemon reports (and the projection carries).
pub const MODEL: &str = "fake";
/// The project directory on the daemon's disk.
pub const PROJECT: &str = "/project";

/// Every clock in the world reads this when the simulation starts.
///
/// # Panics
/// Never: the constant is a valid 48-bit time.
#[must_use]
pub fn epoch() -> UnixMillis {
    UnixMillis::new(1_760_000_000_000).expect("a 48-bit Unix time")
}

/// The daemon's limits (the model replays its runs under the same).
#[must_use]
pub fn limits() -> ServerLimits {
    ServerLimits::v0()
}

/// A host's software, as turmoil runs it.
pub type Software = Pin<Box<dyn Future<Output = turmoil::Result>>>;

const fn any_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)
}

/// The fake Anthropic: q's scripted model, served over HTTP.
pub fn anthropic() -> impl Fn() -> Software {
    || {
        Box::pin(async {
            let clock: Arc<dyn Clock> = Arc::new(SimClock::new(epoch()));
            let listener = net::bind(any_addr(MODEL_PORT)).await?;
            axum::serve(listener, fake::router(clock, World::new())).await?;
            Ok(())
        })
    }
}

/// Creates the project's files on first boot (durably), then reads them.
async fn project_files() -> turmoil::Result<BTreeMap<String, String>> {
    let project = Path::new(PROJECT);
    if fs::stat(project).await?.is_none() {
        fs::create_dir_all(project).await?;
        for (name, text) in FILES {
            fs::write_synced(&project.join(name), text.as_bytes()).await?;
        }
        fs::sync_dir(project).await?;
        fs::sync_dir(Path::new("/")).await?;
    }
    let mut files = BTreeMap::new();
    for (name, _) in FILES {
        let bytes = fs::read(&project.join(name)).await?;
        files.insert(
            name.to_owned(),
            String::from_utf8_lossy(&bytes).into_owned(),
        );
    }
    Ok(files)
}

/// The daemon, booting (and rebooting after each crash) from its disk, with
/// the defects `planted` (none, but in the simulation's mutation tests).
pub fn daemon(ledger: Ledger, seed: u64, planted: Planted) -> impl Fn() -> Software {
    move || {
        let ledger = ledger.clone();
        Box::pin(async move {
            let boot = ledger.boots();
            let files = project_files().await?;
            ledger.record(Entry::Boot { boot, files });
            let limits = limits();
            let clock: Arc<dyn Clock> = Arc::new(SimClock::new(epoch()));
            let model = AnthropicModel::new(
                net::Connector,
                AnthropicConfig {
                    base: format!("http://{ANTHROPIC}:{MODEL_PORT}").parse()?,
                    api_key: ApiKey::new("simulated".to_owned()),
                    model: MODEL.to_owned(),
                },
                &limits,
                Arc::clone(&clock),
            )?;
            let world = World::new();
            let env = SimEnv::new(
                LocalEnv::new(
                    PathBuf::from(PROJECT),
                    Arc::clone(&clock),
                    world.groups().clone(),
                ),
                Arc::clone(&clock),
                ledger.clone(),
            );
            let seams = Seams {
                model: Arc::new(model),
                env: Arc::new(env),
                clock,
                entropy: Arc::new(DaemonEntropy::new(seed ^ u64::from(boot))),
                world,
            };
            let listener = net::bind(any_addr(PORT)).await?;
            let layout = Layout {
                project: PathBuf::from(PROJECT),
            };
            let (_held, store) = daemon::prepare(any_addr(PORT), &layout, &NoLock, &limits).await?;
            let config = ShellConfig {
                limits,
                model: MODEL.to_owned(),
                system: String::new(),
                planted,
            };
            let store = Arc::new(WatchedStore::new(store, ledger, Arc::clone(&seams.clock)));
            daemon::serve(listener, store, seams, config).await?;
            Ok(())
        })
    }
}
