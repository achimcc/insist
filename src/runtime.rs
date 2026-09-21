//! Where the engine meets the network. Everything here holds the one lock —
//! except the metrics (their own `std::sync::Mutex`, read directly by
//! `/metrics`) and the tail of a watchdog ping (the runtime lock covers only
//! the freshness check, never the POST itself).
use crate::alertmanager::{AlertmanagerClient, PostableAlert, WebhookMessage};
use crate::config::Config;
use crate::engine::{AckOutcome, Effects, Engine, Kind, Outgoing, OWN_LABEL};
use crate::metrics::{Metrics, MetricsHandle};
use crate::ntfy::{Action, NtfyClient, Publication, PublishError, StreamLine};
use crate::secret::Secret;
use crate::secrets::Secrets;
use crate::state::Loaded;
use axum::body::Bytes;
use axum::http::StatusCode;
use jiff::{SignedDuration, Timestamp};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(Timestamp::now)
}

pub struct Runtime {
    pub engine: Engine,
    pub metrics: MetricsHandle,
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
        let engine = Engine::new(loaded.state, config.clone(), copy(&secrets.ack_key))?;
        // A restart with open instances already on disk must report them
        // from the first scrape, not only after the next pass touches them.
        metrics.open_instances = engine.open_instances() as u64;
        Ok(Runtime {
            engine,
            ntfy: NtfyClient::new(&config.ntfy_url, copy(&secrets.token))?,
            alertmanager: AlertmanagerClient::new(&config.alertmanager_url)?,
            watchdog: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            metrics: Arc::new(StdMutex::new(metrics)),
            config,
            secrets,
            clock,
        })
    }

    /// A clone of the metrics handle, so `/metrics` can be served without
    /// ever taking the runtime's own (async) lock.
    pub fn metrics_handle(&self) -> MetricsHandle {
        self.metrics.clone()
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
    ///
    /// Raises go out before publishes: the "unacknowledged" mail must reach
    /// Alertmanager even when ntfy is hanging and the publish loop below
    /// stops partway through. And a publish loop that hits an answer about
    /// ntfy ITSELF halts for the rest of this pass (see `halts_the_pass`);
    /// the skipped sends stay due and are retried on the next tick.
    pub async fn process(&mut self, effects: Effects) -> usize {
        for future in &effects.future_starts {
            tracing::warn!(
                "{} (instance {}) starts in the future: startsAt {}, first seen {}; escalating from first seen",
                future.alertname,
                future.id.as_str(),
                future.starts_at,
                future.first_seen
            );
            self.metrics.lock().unwrap().future_starts_total += 1;
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
                // Explicit: Alertmanager 0.31.1 fills an omitted startsAt
                // with endsAt, dating this alert an hour into the future.
                starts_at: Some(now),
                ends_at: Some(now + SignedDuration::from_hours(1)),
            };
            match self.alertmanager.post(&[alert]).await {
                Ok(()) => self.engine.confirm_raise(&raise.id),
                Err(e) => {
                    tracing::error!(
                        "could not raise the unacknowledged alert for {}: {e}",
                        raise.alertname
                    );
                    self.metrics.lock().unwrap().raise_failures_total += 1;
                }
            }
        }

        let mut pending = 0;
        let mut failed_once = 0;
        let mut halted = false;
        for out in &effects.publish {
            if halted {
                // Not attempted: ntfy is presumed hung from the Transport
                // error below. Counted as pending, not as a failure — we
                // never asked and ntfy never refused.
                pending += 1;
                if out.kind == Kind::Once {
                    failed_once += 1;
                }
                continue;
            }
            match self.ntfy.publish(&self.publication(out)).await {
                Ok(()) => {
                    tracing::info!("delivered: {}", out.title);
                    let now = self.now();
                    self.engine.confirm(out, now);
                    self.metrics.lock().unwrap().last_publish_success = Some(now);
                }
                Err(e) => {
                    tracing::error!("not delivered: {}: {e}", out.title);
                    self.metrics.lock().unwrap().publish_failures_total += 1;
                    pending += 1;
                    if out.kind == Kind::Once {
                        failed_once += 1;
                    }
                    if halts_the_pass(&e) {
                        halted = true;
                    }
                }
            }
        }
        let open_instances = self.engine.open_instances() as u64;
        {
            let mut m = self.metrics.lock().unwrap();
            m.publish_pending = pending;
            m.open_instances = open_instances;
        }
        self.save();
        failed_once
    }

    fn save(&mut self) {
        if let Err(e) = self.engine.state().save(&self.config.state_file) {
            tracing::error!("state not saved: {e:#}");
            self.metrics.lock().unwrap().state_save_failures_total += 1;
        }
    }

    pub async fn webhook(&mut self, message: &WebhookMessage) -> usize {
        let now = self.now();
        let effects = self.engine.on_webhook(message, now);
        self.process(effects).await
    }

    /// Reads the silences, then the alerts. A pass counts as successful —
    /// and keeps the dead man's switch fed — only if BOTH answered: a
    /// silence list that cannot be read is never "nothing is muted". The
    /// alerts are still processed when only the silences failed, so
    /// escalation never waits on the other endpoint.
    pub async fn reconcile_once(&mut self) -> bool {
        let silences_read = match self.alertmanager.silences().await {
            Ok(silences) => {
                self.engine.on_silences(&silences);
                let active = silences.iter().filter(|s| s.is_active()).count() as u64;
                self.metrics.lock().unwrap().silences_active = active;
                true
            }
            Err(e) => {
                tracing::error!("reading Alertmanager's silences failed: {e}");
                self.metrics.lock().unwrap().silence_poll_failures_total += 1;
                false
            }
        };
        let fetched_at = self.now();
        match self.alertmanager.alerts().await {
            Ok(alerts) => {
                let now = self.now();
                let effects = self.engine.on_reconcile(&alerts, fetched_at, now);
                if silences_read {
                    self.metrics.lock().unwrap().last_reconcile_success = Some(now);
                }
                self.process(effects).await;
                silences_read
            }
            Err(e) => {
                tracing::error!("reconciliation failed, keeping the last known state: {e}");
                // Silence notices recorded above still go out.
                self.tick_once().await;
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
                self.metrics.lock().unwrap().ack_rejected_total += 1;
            }
        }
        self.engine.state_mut().ack_cursor = Some(line.id);
        let effects = self.engine.tick(now);
        self.process(effects).await;
    }

    /// Checks freshness under the runtime lock, then hands back what a
    /// caller needs to forward the ping *outside* that lock: a cheap client
    /// clone (reqwest::Client is `Arc`-backed), an owned copy of the
    /// watchdog URL, and the metrics and clock to record the outcome with.
    /// Never logs the URL — it is a credential.
    pub fn watchdog_target(&self) -> Result<WatchdogTarget, StatusCode> {
        let now = self.now();
        let last = self.metrics.lock().unwrap().last_reconcile_success;
        let fresh = last.is_some_and(|t| {
            now.as_second() - t.as_second() <= self.config.watchdog_max_age_secs as i64
        });
        if !fresh {
            tracing::error!(
                "watchdog ping withheld: no successful reconciliation within {} s",
                self.config.watchdog_max_age_secs
            );
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        Ok(WatchdogTarget {
            client: self.watchdog.clone(),
            url: copy(&self.secrets.watchdog_url),
            metrics: self.metrics.clone(),
            clock: self.clock.clone(),
        })
    }
}

/// Does this failure say something about **ntfy**, or about this one message?
///
/// An answer about ntfy will be the same answer for every remaining send in
/// the pass, so asking again is at best pointless and at worst the thing that
/// keeps ntfy down. An answer about one message must not hold up the others,
/// or one malformed notification silences the whole house.
///
/// * `Transport` — unreachable or hanging. Already halted before 0.2.4, and
///   for a second reason: with N due sends at up to 10 s each, one pass would
///   hold the single runtime lock for `N * 10s` and starve `/watchdog`,
///   reconciliation and every queued webhook behind it.
/// * **429** — ntfy has said it has had enough. Audit finding B42: every
///   webhook ends with a `tick`, a `tick` offers every open instance that is
///   due, and a failed publish leaves `last_sent` alone. So each webhook
///   retried every earlier alert, and a storm of around 500 became
///   **128100 requests in 100 seconds** — measured on 2026-09-14, against an
///   ntfy that was already rate-limiting. The amplification arrives exactly
///   when it hurts most.
/// * **5xx** — ntfy is broken, not this message.
///
/// Everything else (a 400, a 413, a 401) is about the one notification and
/// leaves the pass running.
///
/// ## What B42 also proposed, and why it is deliberately not here
///
/// The finding asked for a per-instance backoff on top of this. It was
/// written while the quadratic blowup was the observed harm, and the halt
/// above removes that: with `tick_secs = 15`, a refusing ntfy now costs
/// **one** POST every fifteen seconds, whatever the number of open
/// instances. That is not a storm — it is a notification system waiting for
/// its transport, and 15 s is how long it then takes to deliver once ntfy
/// comes back.
///
/// A backoff would trade exactly that away: it would delay the first
/// delivery after a recovery, on purpose, for a saving this halt has already
/// made. The first rule of this program is that it must never make the alert
/// path weaker, and a critical alert arriving late because insist decided to
/// wait is precisely that. If a per-visitor budget ever turns out to need
/// more, the number to change is `tick_secs`, which is configuration and
/// does not touch the escalation ladder at all.
fn halts_the_pass(e: &PublishError) -> bool {
    match e {
        PublishError::Transport => true,
        PublishError::Status(code) => *code == 429 || (500..600).contains(code),
    }
}

/// Everything a watchdog forward needs, copied out from behind the runtime
/// lock. No Debug: the URL is a credential.
pub struct WatchdogTarget {
    client: reqwest::Client,
    url: Secret,
    metrics: MetricsHandle,
    clock: Clock,
}

/// The POST itself, run without holding the runtime lock: `send_watchdog`
/// needs no `&Runtime` at all, only what `watchdog_target` already copied
/// out from behind the lock. Every failed forward is counted; a 2xx moves
/// the success gauge.
pub async fn send_watchdog(target: WatchdogTarget, body: Bytes) -> StatusCode {
    let answer = target
        .client
        .post(target.url.expose())
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await;
    let ok = match answer {
        Ok(r) if r.status().is_success() => true,
        Ok(r) => {
            tracing::error!("dead man's switch answered HTTP {}", r.status().as_u16());
            false
        }
        // The reqwest error is not logged: its Display carries the URL.
        Err(_) => {
            tracing::error!("dead man's switch could not be reached");
            false
        }
    };
    let now = (target.clock)();
    let mut m = target.metrics.lock().unwrap();
    if ok {
        m.last_watchdog_success = Some(now);
        StatusCode::OK
    } else {
        m.watchdog_forward_failures_total += 1;
        StatusCode::BAD_GATEWAY
    }
}
