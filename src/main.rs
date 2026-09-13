use anyhow::Result;
use insist::config::Config;
use insist::ntfy::NtfyClient;
use insist::secret::Secret;
use insist::secrets::Secrets;
use insist::server::{router_stage0, Stage0};
use std::sync::Arc;

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
    // Read once at start: a service that learns at the first alert that it
    // cannot send fails exactly when it is needed.
    let secrets = Secrets::read(&config.secrets_file)?;
    let tz = config.tz()?;
    let ntfy = NtfyClient::new(
        &config.ntfy_url,
        Secret::from(secrets.token.expose().to_string()),
    )?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!("listening on {}", config.listen);
    axum::serve(
        listener,
        router_stage0(Arc::new(Stage0 {
            config,
            secrets,
            ntfy,
            tz,
        })),
    )
    .await?;
    Ok(())
}
