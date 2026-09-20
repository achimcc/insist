use anyhow::Result;
use insist::config::Config;
use insist::runtime::{system_clock, Runtime};
use insist::secret::Secret;
use insist::secrets::Secrets;
use insist::server::{router, App, Shared, WebhookToken};
use insist::state::State;
use insist::tasks::{run_tasks, Tasks};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::MissedTickBehavior;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        // journald keeps escape codes verbatim; a grep over the journal must match plain text
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/etc/insist.toml".into());
    let config = Config::load(std::path::Path::new(&path))?;
    let secrets = Secrets::read(&config.secrets_file)?;
    let loaded = State::load(&config.state_file, jiff::Timestamp::now())?;
    let ack_topic = Secret::from(format!(
        "{}{}",
        secrets.topic.expose(),
        config.ack_topic_suffix
    ));
    let (reconcile, tick) = (config.reconcile_secs, config.tick_secs);
    // Built before the runtime takes config and secrets. Taking the runtime
    // lock three times inside one expression would deadlock on the second.
    let ack_client = insist::ntfy::NtfyClient::new(
        &config.ntfy_url,
        Secret::from(secrets.token.expose().to_string()),
    )?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    // Copied out for the same reason as `ack_client` above: the runtime takes
    // `secrets` on the next line, and the HTTP guard needs the token without
    // reaching through the runtime lock (audit B41).
    let webhook_token = WebhookToken(Secret::from(secrets.webhook_token.expose().to_string()));
    let runtime = Runtime::new(config, secrets, loaded, system_clock())?;
    let metrics = runtime.metrics_handle();
    let shared: Shared = Arc::new(tokio::sync::Mutex::new(runtime));

    // First reconciliation before READY: whatever fired while insist was down
    // is announced now, not a minute later.
    shared.lock().await.reconcile_once().await;
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);

    // Each of these loops forever; run_tasks ends the process if one stops.
    let mut tasks = Tasks::default();
    let s = shared.clone();
    tasks.spawn("reconciliation", async move {
        let mut every = tokio::time::interval(Duration::from_secs(reconcile));
        // A pass bounded by a hanging ntfy can still run long; catching up
        // with a burst of immediate ticks afterwards would only make the
        // next pass worse. Delay just resumes on the next regular boundary.
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            s.lock().await.reconcile_once().await;
        }
    });

    let s = shared.clone();
    tasks.spawn("tick", async move {
        let mut every = tokio::time::interval(Duration::from_secs(tick));
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            s.lock().await.tick_once().await;
            // Only from here: a watchdog ping proves the loop runs AND the
            // lock is not stuck, which a separate timer task would not.
            let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
        }
    });

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let s = shared.clone();
    tasks.spawn("acknowledgement handling", async move {
        while let Some(line) = rx.recv().await {
            s.lock().await.acknowledge(line).await;
        }
    });
    let s = shared.clone();
    let seen = metrics.clone();
    tasks.spawn("acknowledgement stream", async move {
        let mut backoff = 1u64;
        loop {
            let since = s.lock().await.engine.state().ack_cursor.clone();
            let result = ack_client
                .read_acks(
                    &ack_topic,
                    since.as_deref(),
                    Duration::from_secs(120),
                    &tx,
                    &seen,
                )
                .await;
            match result {
                Ok(()) => backoff = 1,
                Err(e) => {
                    tracing::error!("acknowledgement stream: {e}; reconnecting in {backoff} s");
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    });

    tracing::info!("listening");
    let server = axum::serve(
        listener,
        router(App {
            runtime: shared,
            metrics,
            webhook_token,
        }),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    run_tasks(tasks, server).await
}
