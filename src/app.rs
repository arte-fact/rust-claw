use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::channels::AdapterRegistry;
use crate::channels::web::WebChannel;
use crate::config::Config;
use crate::db::{CentralDb, DbError, agent_groups};
use crate::delivery::Delivery;
use crate::protocol::entities::AgentProviderKind;
use crate::router::Router;
use crate::runs::queue::RunQueue;
use crate::runs::supervisor::Supervisor;
use crate::session::{SessionStore, SessionStoreError};
use crate::web::auth::AuthState;
use crate::web::sse::Hub;
use crate::web::{WebState, build_app};

const INBOUND_CHANNEL_CAPACITY: usize = 64;
/// Until the native provider lands (M3), bootstrapped groups run on echo.
const BOOTSTRAP_PROVIDER: AgentProviderKind = AgentProviderKind::Echo;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Session(#[from] SessionStoreError),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// The fully wired daemon: an HTTP app plus the background task set.
pub struct App {
    pub http: axum::Router,
    pub cancel: CancellationToken,
    pub tasks: JoinSet<()>,
}

impl App {
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        while self.tasks.join_next().await.is_some() {}
    }
}

pub async fn build(config: &Config) -> Result<App, AppError> {
    let central = {
        let path = config.central_db_path();
        Arc::new(blocking(move || CentralDb::open(&path)).await?)
    };
    bootstrap_default_agent_group(&central).await?;

    let store = Arc::new(SessionStore::new(config.sessions_dir()));
    recover_stuck_runs(&central, &store).await?;
    let queue = Arc::new(RunQueue::new());
    let hub = Hub::new();
    let web_channel = Arc::new(WebChannel::new(central.clone(), hub.clone()));
    let registry = Arc::new(AdapterRegistry::new(vec![web_channel.clone()]));

    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();

    let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
    for adapter in registry.all() {
        let adapter = adapter.clone();
        let tx = inbound_tx.clone();
        let cancel = cancel.clone();
        tasks.spawn(async move {
            if let Err(error) = adapter.run(tx, cancel).await {
                tracing::error!(channel = adapter.channel_type(), %error, "channel adapter stopped");
            }
        });
    }
    drop(inbound_tx);

    let router = Arc::new(Router::new(central.clone(), store.clone(), queue.clone()));
    tasks.spawn(router.run(inbound_rx, cancel.clone()));

    let commands = Arc::new(crate::commands::Registry::new(central.clone()));

    let supervisor = Arc::new(Supervisor::new(
        central.clone(),
        store.clone(),
        queue.clone(),
        commands.clone(),
        crate::runs::supervisor::RunConfig {
            groups_dir: config.groups_dir(),
            default_endpoint: config.default_endpoint.clone(),
            default_model: config.default_model.clone(),
        },
    ));
    tasks.spawn(supervisor.run(cancel.clone()));

    let delivery = Arc::new(Delivery::new(central.clone(), store.clone(), registry));
    tasks.spawn(delivery.run(cancel.clone()));

    let sweep = Arc::new(crate::sweep::Sweep::new(
        central.clone(),
        store.clone(),
        queue.clone(),
        &config.timezone,
    ));
    tasks.spawn(sweep.run(cancel.clone()));

    let cli_server = Arc::new(crate::cli_server::CliServer::new(
        config.socket_path(),
        commands,
    ));
    {
        let cancel = cancel.clone();
        tasks.spawn(async move {
            if let Err(error) = cli_server.run(cancel).await {
                tracing::error!(%error, "cli server stopped");
            }
        });
    }

    let state = WebState {
        auth: Arc::new(AuthState::from_configured_token(config.auth_token.clone())),
        central,
        web_channel,
        hub,
    };
    Ok(App {
        http: build_app(state),
        cancel,
        tasks,
    })
}

async fn blocking<T>(
    op: impl FnOnce() -> Result<T, DbError> + Send + 'static,
) -> Result<T, AppError>
where
    T: Send + 'static,
{
    crate::blocking::run(op).await
}

/// First boot on an empty database: one agent group so chats have a target.
async fn bootstrap_default_agent_group(central: &Arc<CentralDb>) -> Result<(), AppError> {
    let central = central.clone();
    blocking(move || {
        central.with(|conn| {
            if agent_groups::list(conn)?.is_empty() {
                let mut group = agent_groups::create(conn, "Andy", "andy")?;
                group.agent_provider = Some(BOOTSTRAP_PROVIDER);
                agent_groups::update(conn, &group)?;
                tracing::info!(group = %group.id, "bootstrapped default agent group");
            }
            Ok(())
        })
    })
    .await
}

/// On startup, revert any `processing` message left by a crashed run back to
/// `pending` (§8.2) — a run cannot outlive its supervising process.
async fn recover_stuck_runs(
    central: &Arc<CentralDb>,
    store: &Arc<SessionStore>,
) -> Result<(), AppError> {
    let central = central.clone();
    let store = store.clone();
    crate::blocking::run::<_, AppError, AppError>(move || {
        let active = central.with(crate::db::sessions::list_active)?;
        for session in active {
            let db = store.open(&session.agent_group_id, &session.id)?;
            let reset = db.reset_processing_to_pending()?;
            if reset > 0 {
                tracing::warn!(session = %session.id, count = reset, "reset stuck processing rows");
            }
        }
        Ok(())
    })
    .await
}
