//! Where the engine meets the network. Everything here holds the one lock.
use crate::alertmanager::{AlertmanagerClient, PostableAlert, WebhookMessage};
use crate::config::Config;
use crate::engine::{AckOutcome, Effects, Engine, Kind, Outgoing, OWN_LABEL};
use crate::metrics::Metrics;
use crate::ntfy::{Action, NtfyClient, Publication, StreamLine};
use crate::secret::Secret;
use crate::secrets::Secrets;
use crate::state::Loaded;
use axum::body::Bytes;
use axum::http::StatusCode;
use jiff::{SignedDuration, Timestamp};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(Timestamp::now)
}

pub struct Runtime {
    pub engine: Engine,
    pub metrics: Metrics,
    pub config: Config,
    pub secrets: Secrets,
    pub ntfy: NtfyClient,
    pub alertmanager: AlertmanagerClient,
    pub watchdog: reqwest::Client,
    pub clock: Clock,
}

fn copy(s: &Secret) -> Secret {
    Secret::from(s.expose().to_string())
}

impl Runtime {
    pub fn new(
        config: Config,
        secrets: Secrets,
        loaded: Loaded,
        clock: Clock,
    ) -> anyhow::Result<Runtime> {
        let mut metrics = Metrics::default();
        if let Some(moved) = &loaded.moved_corrupt_to {
            tracing::error!("state file was unreadable, moved to {}; every firing alert will be announced again", moved.display());
            metrics.state_corrupt_total = 1;
        }
        Ok(Runtime {
            engine: Engine::new(loaded.state, config.clone(), copy(&secrets.ack_key))?,
            ntfy: NtfyClient::new(&config.ntfy_url, copy(&secrets.token))?,
            alertmanager: AlertmanagerClient::new(&config.alertmanager_url)?,
            watchdog: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            metrics,
            config,
            secrets,
            clock,
        })
    }

    fn now(&self) -> Timestamp {
        (self.clock)()
    }

    pub fn publication(&self, out: &Outgoing) -> Publication {
        let topic = if out.probe {
            format!(
                "{}{}",
                self.secrets.topic.expose(),
                self.config.probe.topic_suffix
            )
        } else {
            self.secrets.topic.expose().to_string()
        };
        let actions = match &out.button {
            None => vec![],
            Some(body) => vec![Action {
                label: self.config.texts.acknowledge.clone(),
                url: Secret::from(format!(
                    "{}/{}{}",
                    self.ntfy.base(),
                    self.secrets.topic.expose(),
                    self.config.ack_topic_suffix
                )),
                token: copy(&self.secrets.button_token),
                body: body.clone(),
            }],
        };
        Publication {
            topic: Secret::from(topic),
            title: out.title.clone(),
            message: out.message.clone(),
            priority: out.priority,
            tags: out.tags.clone(),
            sequence_id: out.id.as_ref().map(|id| id.as_str().to_string()),
            actions,
        }
    }

    /// Sends what the engine asks for, confirms what went through, saves.
    /// Returns how many stateless (`Once`) sends failed: only those have no
    /// second chance on the next tick and must fail the webhook instead.
    pub async fn process(&mut self, effects: Effects) -> usize {
        let mut pending = 0;
        let mut failed_once = 0;
        for out in &effects.publish {
            match self.ntfy.publish(&self.publication(out)).await {
                Ok(()) => {
                    tracing::info!("delivered: {}", out.title);
                    let now = self.now();
                    self.engine.confirm(out, now);
                    self.metrics.last_publish_success = Some(now);
                }
                Err(e) => {
                    tracing::error!("not delivered: {}: {e}", out.title);
                    self.metrics.publish_failures_total += 1;
                    pending += 1;
                    if out.kind == Kind::Once {
                        failed_once += 1;
                    }
                }
            }
        }
        for raise in &effects.raise {
            let now = self.now();
            let mut labels = BTreeMap::new();
            labels.insert(
                "alertname".to_string(),
                self.config.unacknowledged_alertname.clone(),
            );
            labels.insert("severity".to_string(), "critical".to_string());
            labels.insert(OWN_LABEL.to_string(), "unacknowledged".to_string());
            labels.insert("insist_instance".to_string(), raise.id.as_str().to_string());
            let mut annotations = BTreeMap::new();
            annotations.insert(
                "summary".to_string(),
                self.config
                    .texts
                    .unacknowledged
                    .replace("{alertname}", &raise.alertname)
                    .replace("{minutes}", &raise.minutes.to_string()),
            );
            annotations.insert("description".to_string(), raise.summary.clone());
            let alert = PostableAlert {
                labels,
                annotations,
                ends_at: Some(now + SignedDuration::from_hours(1)),
            };
            match self.alertmanager.post(&[alert]).await {
                Ok(()) => self.engine.confirm_raise(&raise.id),
                Err(e) => tracing::error!(
                    "could not raise the unacknowledged alert for {}: {e}",
                    raise.alertname
                ),
            }
        }
        self.metrics.publish_pending = pending;
        self.metrics.open_instances = self.engine.open_instances() as u64;
        self.save();
        failed_once
    }

    fn save(&mut self) {
        if let Err(e) = self.engine.state().save(&self.config.state_file) {
            tracing::error!("state not saved: {e:#}");
            self.metrics.state_save_failures_total += 1;
        }
    }

    pub async fn webhook(&mut self, message: &WebhookMessage) -> usize {
        let now = self.now();
        let effects = self.engine.on_webhook(message, now);
        self.process(effects).await
    }

    pub async fn reconcile_once(&mut self) -> bool {
        let fetched_at = self.now();
        match self.alertmanager.alerts().await {
            Ok(alerts) => {
                let now = self.now();
                let effects = self.engine.on_reconcile(&alerts, fetched_at, now);
                self.metrics.last_reconcile_success = Some(now);
                self.process(effects).await;
                true
            }
            Err(e) => {
                tracing::error!("reconciliation failed, keeping the last known state: {e}");
                false
            }
        }
    }

    pub async fn tick_once(&mut self) {
        let now = self.now();
        let effects = self.engine.tick(now);
        self.process(effects).await;
    }

    pub async fn acknowledge(&mut self, line: StreamLine) {
        let now = self.now();
        let body = line.message.clone().unwrap_or_default();
        match self.engine.on_ack(&body, now) {
            AckOutcome::Accepted(id) => tracing::info!("acknowledged {}", id.as_str()),
            AckOutcome::AlreadyDone(id) => {
                tracing::info!("already acknowledged or resolved: {}", id.as_str())
            }
            AckOutcome::Unknown(id) => {
                tracing::warn!("acknowledgement for an unknown instance {}", id.as_str())
            }
            AckOutcome::Rejected => {
                // Never the body: it may be anything someone with the button
                // token chose to post.
                tracing::warn!("acknowledgement rejected");
                self.metrics.ack_rejected_total += 1;
            }
        }
        self.engine.state_mut().ack_cursor = Some(line.id);
        let effects = self.engine.tick(now);
        self.process(effects).await;
    }

    pub async fn forward_watchdog(&mut self, body: Bytes) -> StatusCode {
        let now = self.now();
        let fresh = self.metrics.last_reconcile_success.is_some_and(|t| {
            now.as_second() - t.as_second() <= self.config.watchdog_max_age_secs as i64
        });
        if !fresh {
            tracing::error!(
                "watchdog ping withheld: no successful reconciliation within {} s",
                self.config.watchdog_max_age_secs
            );
            return StatusCode::SERVICE_UNAVAILABLE;
        }
        match self
            .watchdog
            .post(self.secrets.watchdog_url.expose())
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => StatusCode::OK,
            Ok(r) => {
                tracing::error!("dead man's switch answered HTTP {}", r.status().as_u16());
                StatusCode::BAD_GATEWAY
            }
            Err(_) => {
                tracing::error!("dead man's switch could not be reached");
                StatusCode::BAD_GATEWAY
            }
        }
    }
}
