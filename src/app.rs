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
    #[error(transparent)]
    Io(#[from] std::io::Error),
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

/// Builds the daemon with a fresh, unfed log buffer — used by tests that don't
/// initialise tracing. Production goes through [`build_with_logs`] so the admin
/// log viewer shares the buffer the tracing layer fills.
pub async fn build(config: &Config) -> Result<App, AppError> {
    build_with_logs(
        config,
        crate::logs::LogBuffer::new(crate::logs::DEFAULT_CAPACITY),
    )
    .await
}

pub async fn build_with_logs(
    config: &Config,
    logs: Arc<crate::logs::LogBuffer>,
) -> Result<App, AppError> {
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

    let supervisor = Arc::new(
        Supervisor::new(
            central.clone(),
            store.clone(),
            queue.clone(),
            commands.clone(),
            crate::runs::supervisor::RunConfig {
                groups_dir: config.groups_dir(),
                default_endpoint: config.default_endpoint.clone(),
                default_model: config.default_model.clone(),
            },
        )
        .with_mcp(connect_web_mcp().await),
    );
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
        commands.clone(),
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
        auth: Arc::new(AuthState::new(resolve_auth_token(config)?)),
        central,
        web_channel,
        hub,
        commands,
        store,
        timezone: config.timezone.clone(),
        groups_dir: config.groups_dir(),
        logs,
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

/// The login token: an explicit `CLAW_AUTH_TOKEN` wins; otherwise a generated
/// token is persisted under `data_dir/auth_token` (0600) so it survives restarts,
/// and printed once. Containers without an env token get a stable, logged token.
fn resolve_auth_token(config: &Config) -> Result<String, AppError> {
    if let Some(token) = &config.auth_token {
        return Ok(token.clone());
    }
    let path = config.data_dir.join("auth_token");
    if let Ok(contents) = std::fs::read_to_string(&path) {
        let token = contents.trim().to_owned();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let token = format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new()).to_lowercase();
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::warn!(
        token = %token,
        path = %path.display(),
        "CLAW_AUTH_TOKEN not set — generated and saved a login token; set CLAW_AUTH_TOKEN to override"
    );
    Ok(token)
}

/// Spawns the bundled web-search MCP server (M12) — best-effort: if the binary is
/// missing or the handshake fails, agents simply run without the web tools.
async fn connect_web_mcp() -> Option<Arc<crate::mcp::McpClient>> {
    use crate::mcp::{McpClient, WEB_SERVER_COMMAND, WEB_SERVER_NAME};
    match McpClient::spawn(WEB_SERVER_NAME, WEB_SERVER_COMMAND, &[]).await {
        Ok(client) => {
            tracing::info!(
                tools = client.tools().len(),
                "web-search mcp server connected"
            );
            Some(Arc::new(client))
        }
        Err(error) => {
            tracing::warn!(%error, "web-search mcp server unavailable; agents run without web tools");
            None
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config_in(dir: &std::path::Path, token: Option<&str>) -> Config {
        Config::from_lookup(|var| match var {
            "CLAW_DATA_DIR" => Some(dir.to_string_lossy().into_owned()),
            "CLAW_AUTH_TOKEN" => token.map(str::to_owned),
            _ => None,
        })
        .expect("config")
    }

    #[test]
    fn generated_token_persists_across_restarts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_in(tmp.path(), None);
        let first = resolve_auth_token(&config).expect("first");
        let second = resolve_auth_token(&config).expect("second");
        assert_eq!(first, second, "the persisted token must be reused");
        assert!(!first.is_empty());
        assert!(config.data_dir.join("auth_token").is_file());
    }

    #[test]
    fn explicit_token_wins_and_is_not_persisted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_in(tmp.path(), Some("explicit"));
        assert_eq!(resolve_auth_token(&config).expect("token"), "explicit");
        assert!(
            !config.data_dir.join("auth_token").exists(),
            "an env-provided token must not be written to disk"
        );
    }
}
