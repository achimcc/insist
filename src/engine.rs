//! The state machine. No IO: it turns what happened (a webhook, an API
//! answer, a tick, a button press) into what should be sent, and only a
//! confirmation after a successful send moves an instance forward.
use crate::alertmanager::{GettableAlert, Labels, WebhookMessage};
use crate::config::Config;
use crate::instance::InstanceId;
use crate::ladder::{self, Due, Progress};
use crate::mac;
use crate::render;
use crate::secret::Secret;
use crate::state::{Instance, State};
use jiff::{tz::TimeZone, Timestamp};
use std::collections::BTreeSet;

/// Alerts carrying this label were raised by insist itself (the
/// "unacknowledged" mail). They are routed to mail only and never handled.
pub const OWN_LABEL: &str = "insist";

/// Clock skew up to this much between a producer and insist is noise, not a
/// fault worth a log line.
pub const FUTURE_START_TOLERANCE_SECS: i64 = 60;

/// After this many refusals of one send that were about the send itself (a
/// 4xx other than 429, or a 500), it goes out as a minimal text instead:
/// the alert name or the silence id, and a pointer to Alertmanager. Nothing
/// a producer wrote can then be what keeps it from being delivered.
pub const MINIMAL_AFTER: u32 = 3;

/// After this many, a silence notice is given up (see `Engine::failed`).
/// An alert never is.
pub const GIVE_UP_AFTER: u32 = 6;

/// How long a silence comment may be inside the notice, before the whole
/// message is cut to `render::MESSAGE_MAX_BYTES`. Short enough that the
/// fixed part of the text always survives.
const COMMENT_MAX_BYTES: usize = 1500;
const MATCHERS_MAX_CHARS: usize = 150;
const CREATED_BY_MAX_CHARS: usize = 64;
const ALERTNAME_MINIMAL_MAX_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// The step index and whether this send is the night substitute (see
    /// `ladder::Due::quiet`) — carried explicitly so `confirm` can record it
    /// on the instance without re-deriving it from the priority number.
    Step(usize, bool),
    Acknowledged,
    Resolved,
    Once,
    /// The notice about an Alertmanager silence, by silence id.
    Silence(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    pub id: Option<InstanceId>,
    pub kind: Kind,
    pub probe: bool,
    pub title: String,
    pub message: String,
    pub priority: u8,
    pub tags: Vec<String>,
    pub button: Option<String>,
}

impl Outgoing {
    /// Every notification leaves the engine through here: control
    /// characters gone, title and message cut to what ntfy stores as a
    /// message (audit 3, B78).
    fn bounded(mut self) -> Outgoing {
        self.title = render::cut_chars(&render::clean(&self.title), render::TITLE_MAX_CHARS);
        self.message = render::cut_bytes(&render::clean(&self.message), render::MESSAGE_MAX_BYTES);
        self
    }

    /// The key a failed send is counted under: the same notification on
    /// the next tick has the same key. None for `Once`, which has no next
    /// tick (Alertmanager retries the webhook instead).
    fn failure_key(&self) -> Option<String> {
        let id = self.id.as_ref().map(|i| i.as_str());
        match (&self.kind, id) {
            (Kind::Silence(sid), _) => Some(format!("silence:{sid}")),
            (Kind::Step(..), Some(id)) => Some(format!("{id}:step")),
            (Kind::Acknowledged, Some(id)) => Some(format!("{id}:acknowledged")),
            (Kind::Resolved, Some(id)) => Some(format!("{id}:resolved")),
            _ => None,
        }
    }
}

/// Refusals of one send that were about the send itself, since its last
/// delivery. Kept in memory only: after a restart a send gets its
/// `MINIMAL_AFTER` tries again, which costs a minute, not a notification.
#[derive(Debug, Clone)]
struct Failures {
    count: u32,
    since: Timestamp,
    /// Whether ntfy answered a 4xx (other than 429) at least once: a
    /// verdict on the message, where a 500 may also be ntfy being broken.
    refused: bool,
}

/// How a send failed, as far as the engine cares. Answers about ntfy
/// itself (unreachable, 429, 502 to 504) are not reported here at all: they
/// say nothing about this message, and the pass stops on them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Failure {
    /// A 4xx other than 429: ntfy will not take this message.
    Refused,
    /// A 500 or another 5xx that is not a proxy's: this message, or ntfy
    /// broken — only a delivery of something else tells which.
    ServerError,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Unacknowledged {
    pub id: InstanceId,
    pub alertname: String,
    pub summary: String,
    pub minutes: i64,
}

/// An instance recorded for the first time whose `startsAt` lies more than
/// `FUTURE_START_TOLERANCE_SECS` ahead of insist's clock. Reported once, so
/// the runtime can log and count the producer's fault.
#[derive(Debug, Clone, PartialEq)]
pub struct FutureStart {
    pub id: InstanceId,
    pub alertname: String,
    pub starts_at: Timestamp,
    pub first_seen: Timestamp,
}

#[derive(Debug, Default, PartialEq)]
pub struct Effects {
    pub publish: Vec<Outgoing>,
    pub raise: Vec<Unacknowledged>,
    pub future_starts: Vec<FutureStart>,
}

impl Effects {
    fn extend(&mut self, other: Effects) {
        self.publish.extend(other.publish);
        self.raise.extend(other.raise);
        self.future_starts.extend(other.future_starts);
    }
}

#[derive(Debug, PartialEq)]
pub enum AckOutcome {
    Accepted(InstanceId),
    AlreadyDone(InstanceId),
    Unknown(InstanceId),
    Rejected,
}

pub struct Engine {
    state: State,
    config: Config,
    tz: TimeZone,
    ack_key: Secret,
    ack_key_previous: Option<Secret>,
    failures: std::collections::BTreeMap<String, Failures>,
    last_delivery: Option<Timestamp>,
}

impl Engine {
    pub fn new(state: State, config: Config, ack_key: Secret) -> anyhow::Result<Engine> {
        let tz = config.tz()?;
        Ok(Engine {
            state,
            config,
            tz,
            ack_key,
            ack_key_previous: None,
            failures: Default::default(),
            last_delivery: None,
        })
    }

    /// The HMAC key before the last rotation: buttons signed with it are
    /// still accepted, so a rotation does not disarm every button on a
    /// phone until the next ladder step replaces it (audit 3, B123).
    pub fn with_previous_ack_key(mut self, key: Option<Secret>) -> Engine {
        self.ack_key_previous = key;
        self
    }

    fn failed_often(&self, key: Option<String>, times: u32) -> bool {
        key.and_then(|k| self.failures.get(&k))
            .is_some_and(|f| f.count >= times)
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn open_instances(&self) -> usize {
        self.state
            .instances
            .values()
            .filter(|i| i.resolved_at.is_none() && i.acknowledged_at.is_none())
            .count()
    }

    fn is_own(labels: &Labels) -> bool {
        labels.contains_key(OWN_LABEL)
    }

    fn is_probe(&self, labels: &Labels) -> bool {
        labels.get(&self.config.probe.label) == Some(&self.config.probe.value)
    }

    /// Whether an API alert's receivers intersect `config.receivers`. `GET
    /// /api/v2/alerts` also lists alerts routed to receivers insist never
    /// gets a webhook for (mail-only, say); those must not become instances.
    fn has_matching_receiver(&self, alert: &GettableAlert) -> bool {
        alert
            .receivers
            .iter()
            .any(|r| self.config.receivers.contains(&r.name))
    }

    /// Creates the instance if it is new, and always updates `ends_at` from
    /// a real (non-zero) value — never overwriting a known deadline with an
    /// unknown one, since a webhook never carries a real `endsAt` and must
    /// not erase what an earlier API answer established. A new instance
    /// whose `startsAt` lies in the future is reported in `effects` — once,
    /// here, where it is first recorded.
    #[allow(clippy::too_many_arguments)]
    fn upsert(
        &mut self,
        fingerprint: &str,
        starts_at: Timestamp,
        labels: &Labels,
        annotations: &Labels,
        ends_at: Option<Timestamp>,
        now: Timestamp,
        effects: &mut Effects,
    ) -> Option<InstanceId> {
        let (ladder, _) = self.config.ladder_for(labels)?;
        let id = InstanceId::of(fingerprint, starts_at);
        let probe = self.is_probe(labels);
        if !self.state.instances.contains_key(&id)
            && starts_at.as_second() - now.as_second() > FUTURE_START_TOLERANCE_SECS
        {
            effects.future_starts.push(FutureStart {
                id: id.clone(),
                alertname: render::alertname(labels),
                starts_at,
                first_seen: now,
            });
        }
        let instance = self
            .state
            .instances
            .entry(id.clone())
            .or_insert_with(|| Instance {
                fingerprint: fingerprint.to_string(),
                starts_at,
                ends_at: None,
                first_seen: now,
                alertname: render::alertname(labels),
                ladder,
                summary: render::summary(labels, annotations),
                probe,
                last_step: None,
                last_sent: None,
                acknowledged_at: None,
                acknowledgement_sent: false,
                resolved_at: None,
                suppressed: false,
                unacknowledged_raised: false,
                last_quiet: false,
            });
        if let Some(e) = ends_at {
            instance.ends_at = Some(e);
        }
        Some(id)
    }

    pub fn on_webhook(&mut self, message: &WebhookMessage, now: Timestamp) -> Effects {
        let mut effects = Effects::default();
        for alert in &message.alerts {
            if Self::is_own(&alert.labels) {
                continue;
            }
            let resolved = alert.status == "resolved";
            if self.config.ladder_for(&alert.labels).is_none() {
                if !resolved {
                    effects.publish.push(
                        Outgoing {
                            id: None,
                            kind: Kind::Once,
                            probe: self.is_probe(&alert.labels),
                            title: render::alertname(&alert.labels),
                            message: render::summary(&alert.labels, &alert.annotations),
                            priority: 2,
                            tags: vec!["information_source".into()],
                            button: None,
                        }
                        .bounded(),
                    );
                }
                continue;
            }
            if resolved {
                let id = InstanceId::of(&alert.fingerprint, alert.starts_at);
                if let Some(instance) = self.state.instances.get_mut(&id) {
                    instance.resolved_at.get_or_insert(now);
                }
            } else {
                let ends_at = crate::alertmanager::known_ends_at(alert.ends_at);
                if let Some(id) = self.upsert(
                    &alert.fingerprint,
                    alert.starts_at,
                    &alert.labels,
                    &alert.annotations,
                    ends_at,
                    now,
                    &mut effects,
                ) {
                    // Alertmanager never sends a webhook for a silenced or
                    // inhibited alert, so its arrival is itself proof the
                    // instance is no longer suppressed.
                    if let Some(instance) = self.state.instances.get_mut(&id) {
                        instance.suppressed = false;
                    }
                }
            }
        }
        effects.extend(self.tick(now));
        effects
    }

    /// `fetched_at` is when the API request was sent. An instance first seen
    /// after that moment may simply be missing from an older answer.
    pub fn on_reconcile(
        &mut self,
        alerts: &[GettableAlert],
        fetched_at: Timestamp,
        now: Timestamp,
    ) -> Effects {
        let mut effects = Effects::default();
        let mut present = BTreeSet::new();
        for alert in alerts {
            if Self::is_own(&alert.labels) {
                continue;
            }
            if !self.has_matching_receiver(alert) {
                continue;
            }
            let ends_at = crate::alertmanager::known_ends_at(alert.ends_at);
            if let Some(id) = self.upsert(
                &alert.fingerprint,
                alert.starts_at,
                &alert.labels,
                &alert.annotations,
                ends_at,
                now,
                &mut effects,
            ) {
                if let Some(instance) = self.state.instances.get_mut(&id) {
                    instance.suppressed = alert.status.state == "suppressed";
                }
                present.insert(id);
            }
        }
        // Alertmanager keeps alerts in memory only: after a restart with no
        // alerts re-posted yet, the API answers 200 []. Absence resolves an
        // instance only once its last known endsAt has passed — otherwise a
        // restart during an outage would announce a false all-clear.
        // Measured on the live deployment: restarting the Alertmanager/
        // Prometheus host re-fires every rule with a new startsAt, so this
        // is a new instance, not a resend of the old one — the old one just
        // resolves here once its endsAt passes, unacknowledged.
        for (id, instance) in self.state.instances.iter_mut() {
            if instance.resolved_at.is_none()
                && !present.contains(id)
                && instance.first_seen <= fetched_at
                && instance.ends_at.is_none_or(|e| e <= now)
            {
                instance.resolved_at = Some(now);
            }
        }
        effects.extend(self.tick(now));
        effects
    }

    pub fn tick(&mut self, now: Timestamp) -> Effects {
        // Resolved before anything went out: nothing to take back on a
        // phone. But an acknowledgement proves a notification existed even
        // if last_step was never confirmed (a defensive rule, not a path
        // the public API normally produces), so Resolved must replace it.
        self.state.instances.retain(|_, i| {
            !(i.resolved_at.is_some() && i.last_step.is_none() && i.acknowledged_at.is_none())
        });

        // Counts for sends that no longer exist are dropped with them.
        let (instances, silences) = (&self.state.instances, &self.state.silences);
        self.failures
            .retain(|key, _| match key.strip_prefix("silence:") {
                Some(sid) => silences.contains_key(sid),
                None => key
                    .split_once(':')
                    .and_then(|(id, _)| InstanceId::parse(id))
                    .is_some_and(|id| instances.contains_key(&id)),
            });

        let mut effects = Effects::default();
        for (id, instance) in &self.state.instances {
            if instance.resolved_at.is_some() {
                effects.publish.push(self.resolved(id, instance));
                continue;
            }
            if let Some(at) = instance.acknowledged_at {
                if !instance.acknowledgement_sent {
                    effects.publish.push(self.acknowledged(id, instance, at));
                }
                continue;
            }
            if instance.suppressed {
                continue;
            }
            // A ladder removed from the configuration must not silence an
            // open instance: fall back to warning.
            let Some(ladder) = self
                .config
                .ladders
                .get(&instance.ladder)
                .or_else(|| self.config.ladders.get("warning"))
            else {
                continue;
            };
            let progress = Progress {
                last_step: instance.last_step,
                last_sent: instance.last_sent,
                quiet: instance.last_quiet,
            };
            if let Some(due) = ladder::due(
                ladder,
                instance.effective_start(),
                &progress,
                now,
                &self.config.night,
                &self.tz,
            ) {
                effects.publish.push(self.step(id, instance, &due, now));
                if due.raise_unacknowledged && !instance.unacknowledged_raised {
                    effects.raise.push(Unacknowledged {
                        id: id.clone(),
                        alertname: instance.alertname.clone(),
                        summary: instance.summary.clone(),
                        minutes: (now.as_second() - instance.effective_start().as_second()) / 60,
                    });
                }
            }
        }
        // Silence notices LAST (audit 3, B78): in 0.3.0 they stood first,
        // and one ntfy would not take stopped every pass before any alert.
        for (sid, notice) in &self.state.silences {
            if !notice.announced {
                effects.publish.push(self.silence_notice(sid, notice));
            }
        }
        effects.publish = effects.publish.into_iter().map(Outgoing::bounded).collect();
        effects
    }

    /// Minimal: `title` stays, the message says only where the details are.
    fn minimal_if_refused(&self, mut out: Outgoing, title: String) -> Outgoing {
        if self.failed_often(out.failure_key(), MINIMAL_AFTER) {
            out.title = title;
            out.message = self.config.texts.details_elsewhere.clone();
        }
        out
    }

    fn short_alertname(i: &Instance) -> String {
        render::cut_chars(&render::clean(&i.alertname), ALERTNAME_MINIMAL_MAX_CHARS)
    }

    fn step(&self, id: &InstanceId, i: &Instance, due: &Due, now: Timestamp) -> Outgoing {
        let loud = i.ladder == "critical" || i.probe;
        let out = Outgoing {
            id: Some(id.clone()),
            kind: Kind::Step(due.step, due.quiet),
            probe: i.probe,
            title: i.alertname.clone(),
            message: format!(
                "{} ({} {})",
                i.summary,
                self.config.texts.since,
                render::clock(i.effective_start(), &self.tz)
            ),
            priority: due.priority,
            tags: vec![if loud { "rotating_light" } else { "warning" }.into()],
            button: Some(mac::button_body(
                &self.ack_key,
                id,
                now + jiff::SignedDuration::from_secs(self.config.button_valid_secs as i64),
            )),
        };
        self.minimal_if_refused(out, Self::short_alertname(i))
    }

    /// Records what `GET /api/v2/silences` answered (B15 of the homeserver
    /// audit, 2026-09-15). A silence on `alertname=~".+"` switches every
    /// alert off, and a rule inside Alertmanager that watched for silences
    /// could be silenced the same way — so the notice goes from here
    /// straight to ntfy, past Alertmanager's routing.
    ///
    /// Only `active` counts: expired silences stay in the list, pending
    /// ones mute nothing yet. A silence is told once. What was told and is
    /// no longer active is forgotten; what was NOT told yet stays until it
    /// is, even if the silence has ended meanwhile — it still happened.
    pub fn on_silences(&mut self, silences: &[crate::alertmanager::GettableSilence]) {
        let active: BTreeSet<&str> = silences
            .iter()
            .filter(|s| s.is_active())
            .map(|s| s.id.as_str())
            .collect();
        for s in silences.iter().filter(|s| s.is_active()) {
            let notice = self.state.silences.entry(s.id.clone()).or_insert_with(|| {
                crate::state::SilenceNotice {
                    matchers: s.matchers_text(),
                    ends_at: s.ends_at,
                    created_by: s.created_by.clone(),
                    comment: s.comment.clone(),
                    announced: false,
                    active: true,
                }
            });
            notice.ends_at = s.ends_at;
        }
        for (id, notice) in self.state.silences.iter_mut() {
            notice.active = active.contains(id.as_str());
        }
        self.state.silences.retain(|_, n| n.active || !n.announced);
    }

    /// Everything in it but the fixed words was written by whoever set the
    /// silence. So each such field is cleaned and cut on its own, and the
    /// comment goes last, in quotes it cannot close (a `"` in it becomes
    /// `'`): "Probe, ignore this — Achim" stays visibly a quotation, and
    /// the time and author in front of it always survive (audit 3, B122).
    fn silence_notice(&self, sid: &str, n: &crate::state::SilenceNotice) -> Outgoing {
        let until = n
            .ends_at
            .to_zoned(self.tz.clone())
            .strftime("%Y-%m-%d %H:%M")
            .to_string();
        let field = |s: &str, max| render::cut_chars(&render::clean(s), max);
        let comment =
            render::cut_bytes(&render::clean(&n.comment), COMMENT_MAX_BYTES).replace('"', "'");
        let texts = &self.config.texts;
        let out = Outgoing {
            id: None,
            kind: Kind::Silence(sid.to_string()),
            probe: false,
            title: texts
                .silenced
                .replace("{matchers}", &field(&n.matchers, MATCHERS_MAX_CHARS)),
            message: texts
                .silence_detail
                .replace("{until}", &until)
                .replace("{by}", &field(&n.created_by, CREATED_BY_MAX_CHARS))
                .replace("{comment}", &format!("\"{comment}\"")),
            // It will not escalate: this one notice is all there is.
            priority: 5,
            tags: vec!["mute".into()],
            button: None,
        };
        // The id is Alertmanager's own uuid, not something a person typed.
        let minimal = texts.silenced.replace("{matchers}", &render::clean(sid));
        self.minimal_if_refused(out, minimal)
    }

    fn acknowledged(&self, id: &InstanceId, i: &Instance, at: Timestamp) -> Outgoing {
        let title = |name: &str| {
            format!(
                "{} {}: {}",
                self.config.texts.acknowledged,
                render::clock(at, &self.tz),
                name
            )
        };
        let out = Outgoing {
            id: Some(id.clone()),
            kind: Kind::Acknowledged,
            probe: i.probe,
            title: title(&i.alertname),
            message: i.summary.clone(),
            priority: 2,
            tags: vec!["ballot_box_with_check".into()],
            button: None,
        };
        self.minimal_if_refused(out, title(&Self::short_alertname(i)))
    }

    fn resolved(&self, id: &InstanceId, i: &Instance) -> Outgoing {
        let title = |name: &str| format!("{}: {}", self.config.texts.resolved, name);
        let out = Outgoing {
            id: Some(id.clone()),
            kind: Kind::Resolved,
            probe: i.probe,
            title: title(&i.alertname),
            message: i.summary.clone(),
            priority: 2,
            tags: vec!["white_check_mark".into()],
            button: None,
        };
        self.minimal_if_refused(out, title(&Self::short_alertname(i)))
    }

    /// A send ntfy refused for a reason about the send itself. Counted, so
    /// that the next ticks offer it as a minimal text (`MINIMAL_AFTER`).
    ///
    /// Returns true when the send is given up — only ever a silence notice,
    /// after `GIVE_UP_AFTER` refusals, and only when those refusals were
    /// about the message: a 4xx, or 500s while ntfy DID deliver something
    /// else since the first of them. An ntfy that answers 500 to everything
    /// is broken, and the notice waits for it. A given-up notice counts as
    /// told: it no longer holds its place in the state once the silence
    /// ends (in 0.3.0 it stayed forever, across restarts). An alert is
    /// never given up; it stays pending and `InsistSendetNicht` sees it.
    pub fn failed(&mut self, out: &Outgoing, failure: Failure, now: Timestamp) -> bool {
        let Some(key) = out.failure_key() else {
            return false;
        };
        let f = self.failures.entry(key.clone()).or_insert(Failures {
            count: 0,
            since: now,
            refused: false,
        });
        f.count += 1;
        f.refused |= failure == Failure::Refused;
        let about_the_message = f.refused || self.last_delivery.is_some_and(|d| d > f.since);
        let Kind::Silence(sid) = &out.kind else {
            return false;
        };
        if f.count < GIVE_UP_AFTER || !about_the_message {
            return false;
        }
        self.failures.remove(&key);
        if let Some(n) = self.state.silences.get_mut(sid) {
            n.announced = true;
        }
        self.state.silences.retain(|_, n| n.active || !n.announced);
        true
    }

    pub fn confirm(&mut self, out: &Outgoing, now: Timestamp) {
        self.last_delivery = Some(now);
        if let Some(key) = out.failure_key() {
            self.failures.remove(&key);
        }
        if let Kind::Silence(sid) = &out.kind {
            let ended = match self.state.silences.get_mut(sid) {
                Some(n) => {
                    n.announced = true;
                    !n.active
                }
                None => false,
            };
            if ended {
                self.state.silences.remove(sid);
            }
            return;
        }
        let Some(id) = &out.id else { return };
        match out.kind {
            Kind::Resolved => {
                self.state.instances.remove(id);
            }
            Kind::Acknowledged => {
                if let Some(i) = self.state.instances.get_mut(id) {
                    i.acknowledgement_sent = true;
                }
            }
            Kind::Step(step, quiet) => {
                if let Some(i) = self.state.instances.get_mut(id) {
                    i.last_step = Some(step);
                    i.last_sent = Some(now);
                    i.last_quiet = quiet;
                }
            }
            Kind::Once | Kind::Silence(_) => {}
        }
    }

    pub fn confirm_raise(&mut self, id: &InstanceId) {
        if let Some(i) = self.state.instances.get_mut(id) {
            i.unacknowledged_raised = true;
        }
    }

    pub fn on_ack(&mut self, body: &str, now: Timestamp) -> AckOutcome {
        let Some(id) = mac::verify(&self.ack_key, self.ack_key_previous.as_ref(), body, now) else {
            return AckOutcome::Rejected;
        };
        match self.state.instances.get_mut(&id) {
            None => AckOutcome::Unknown(id),
            Some(i) if i.acknowledged_at.is_some() || i.resolved_at.is_some() => {
                AckOutcome::AlreadyDone(id)
            }
            Some(i) => {
                i.acknowledged_at = Some(now);
                AckOutcome::Accepted(id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alertmanager::{GettableAlert, WebhookMessage};
    use crate::config::tests::MINIMAL;
    use crate::state::State;
    use jiff::{SignedDuration, Timestamp};

    const KEY: &str = "3f9a0c1e5b7d2f4a6c8e0b1d3f5a7c9e";

    fn recorded(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/fixtures/alertmanager-0.31.1/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }
    fn engine() -> Engine {
        Engine::new(
            State::default(),
            Config::from_toml(MINIMAL).unwrap(),
            Secret::from(KEY.to_string()),
        )
        .unwrap()
    }
    fn single() -> WebhookMessage {
        serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap()
    }
    fn plus(t: Timestamp, secs: i64) -> Timestamp {
        t + SignedDuration::from_secs(secs)
    }
    /// Constructed from the recorded single alert: only startsAt changes.
    fn single_starting_at(starts_at: Timestamp) -> WebhookMessage {
        let mut v: serde_json::Value =
            serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
        v["alerts"][0]["startsAt"] = starts_at.to_string().into();
        serde_json::from_value(v).unwrap()
    }
    fn send_all(e: &mut Engine, fx: &Effects, now: Timestamp) {
        for o in &fx.publish {
            e.confirm(o, now);
        }
        for r in &fx.raise {
            e.confirm_raise(&r.id);
        }
    }

    fn recorded_silences(name: &str) -> Vec<crate::alertmanager::GettableSilence> {
        serde_json::from_str(&recorded(name)).unwrap()
    }
    fn silence_notices(fx: &Effects) -> Vec<&Outgoing> {
        fx.publish
            .iter()
            .filter(|o| matches!(o.kind, Kind::Silence(_)))
            .collect()
    }

    #[test]
    fn an_active_silence_is_announced_once_loudly_and_a_pending_one_not_at_all() {
        let mut e = engine();
        let list = recorded_silences("silences-active-and-pending.json");
        let now = plus(list.iter().find(|s| s.is_active()).unwrap().starts_at, 5);

        e.on_silences(&list);
        let fx = e.tick(now);
        let notices = silence_notices(&fx);
        assert_eq!(notices.len(), 1, "the pending silence mutes nothing yet");
        let n = notices[0];
        assert!(n.title.contains(r#"alertname=~".+""#), "{}", n.title);
        assert!(n.message.contains("everything"), "{}", n.message);
        assert!(n.message.contains("record-silences.sh"), "{}", n.message);
        assert!(n.priority >= 4, "a muted alert path is not a quiet event");
        assert!(!n.probe);
        assert!(n.button.is_none(), "there is nothing to acknowledge");
        send_all(&mut e, &fx, now);

        e.on_silences(&list);
        assert!(
            silence_notices(&e.tick(plus(now, 60))).is_empty(),
            "announced once per silence, not on every pass"
        );
    }

    #[test]
    fn an_undelivered_silence_notice_stays_due_even_after_the_silence_ended() {
        let mut e = engine();
        let active = recorded_silences("silences-active-and-pending.json");
        let now = plus(active.iter().find(|s| s.is_active()).unwrap().starts_at, 5);
        e.on_silences(&active);
        assert_eq!(silence_notices(&e.tick(now)).len(), 1);
        // ntfy did not take it: nothing confirmed. Meanwhile the silence
        // was expired again — it still happened, and still gets told.
        e.on_silences(&recorded_silences("silences-after-expire.json"));
        let fx = e.tick(plus(now, 15));
        assert_eq!(silence_notices(&fx).len(), 1);
        send_all(&mut e, &fx, plus(now, 15));
        assert!(silence_notices(&e.tick(plus(now, 30))).is_empty());
        assert!(e.state().silences.is_empty(), "nothing left to remember");
    }

    #[test]
    fn a_delivered_silence_that_ended_is_forgotten() {
        let mut e = engine();
        let active = recorded_silences("silences-active-and-pending.json");
        let now = plus(active.iter().find(|s| s.is_active()).unwrap().starts_at, 5);
        e.on_silences(&active);
        let fx = e.tick(now);
        send_all(&mut e, &fx, now);
        assert_eq!(e.state().silences.len(), 1, "remembered while active");
        e.on_silences(&recorded_silences("silences-after-expire.json"));
        assert!(e.state().silences.is_empty());
    }

    #[test]
    fn a_recorded_critical_alert_climbs_the_ladder_until_acknowledged() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;

        let fx = e.on_webhook(&w, t0);
        assert_eq!(fx.publish.len(), 1);
        let first = &fx.publish[0];
        assert_eq!(first.kind, Kind::Step(0, false));
        assert_eq!(first.priority, 4);
        assert_eq!(first.title, "UnitFehlgeschlagen");
        let body = first.button.clone().expect("a button");
        assert_eq!(
            crate::mac::verify(&Secret::from(KEY.to_string()), None, &body, t0),
            first.id
        );
        send_all(&mut e, &fx, t0);

        assert!(e.tick(plus(t0, 60)).publish.is_empty());

        let fx = e.tick(plus(t0, 900));
        assert_eq!(
            (fx.publish[0].kind.clone(), fx.publish[0].priority),
            (Kind::Step(1, false), 5)
        );
        send_all(&mut e, &fx, plus(t0, 900));

        let fx = e.tick(plus(t0, 3600));
        assert_eq!(fx.publish[0].kind, Kind::Step(2, false));
        assert_eq!(fx.raise.len(), 1);
        assert_eq!(fx.raise[0].minutes, 60);
        send_all(&mut e, &fx, plus(t0, 3600));

        let fx = e.tick(plus(t0, 3900));
        assert_eq!(fx.publish[0].kind, Kind::Step(2, false));
        assert!(
            fx.raise.is_empty(),
            "the unacknowledged mail is raised once"
        );
        send_all(&mut e, &fx, plus(t0, 3900));

        assert!(matches!(
            e.on_ack(&body, plus(t0, 4000)),
            AckOutcome::Accepted(_)
        ));
        let fx = e.tick(plus(t0, 4000));
        assert_eq!(fx.publish.len(), 1);
        assert_eq!(fx.publish[0].kind, Kind::Acknowledged);
        assert!(fx.publish[0].button.is_none());
        assert_eq!(fx.publish[0].priority, 2);
        assert_eq!(
            fx.publish[0].id, first.id,
            "same sequence id replaces the notification"
        );
        send_all(&mut e, &fx, plus(t0, 4000));

        assert!(
            e.tick(plus(t0, 9000)).publish.is_empty(),
            "silence after acknowledgement"
        );
        assert!(matches!(
            e.on_ack(&body, plus(t0, 9000)),
            AckOutcome::AlreadyDone(_)
        ));
    }

    // --- Audit 3 (2026-09-27): B78, B122, B123 ---

    fn silence_with_comment(comment: &str) -> Vec<crate::alertmanager::GettableSilence> {
        let mut list = recorded_silences("silences-active-and-pending.json");
        for s in list.iter_mut().filter(|s| s.is_active()) {
            s.comment = comment.to_string();
        }
        list
    }

    #[test]
    fn a_silence_notice_goes_behind_every_alert_in_the_pass() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        e.on_silences(&silence_with_comment("Wartung"));
        let fx = e.on_webhook(&w, t0);
        let kinds: Vec<_> = fx.publish.iter().map(|o| o.kind.clone()).collect();
        assert_eq!(kinds.len(), 2, "{kinds:?}");
        assert_eq!(kinds[0], Kind::Step(0, false), "{kinds:?}");
        assert!(matches!(kinds[1], Kind::Silence(_)), "{kinds:?}");
    }

    #[test]
    fn every_notification_is_cleaned_and_cut() {
        let mut e = engine();
        let mut v: serde_json::Value =
            serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
        v["alerts"][0]["annotations"]["summary"] =
            format!("line one\nline two\u{1b}[31m {}", "x".repeat(10_000)).into();
        v["alerts"][0]["labels"]["alertname"] = "N".repeat(500).into();
        let w: WebhookMessage = serde_json::from_value(v).unwrap();
        let fx = e.on_webhook(&w, w.alerts[0].starts_at);
        let o = &fx.publish[0];
        assert!(
            o.message.len() <= render::MESSAGE_MAX_BYTES,
            "{}",
            o.message.len()
        );
        assert!(o.title.chars().count() <= render::TITLE_MAX_CHARS);
        assert!(
            o.message.starts_with("line one line two [31m"),
            "{}",
            &o.message[..40]
        );
    }

    #[test]
    fn a_step_ntfy_keeps_refusing_goes_out_minimal_with_its_button_and_is_never_given_up() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let mut fx = e.on_webhook(&w, t0);
        for n in 1..=20 {
            let out = fx.publish[0].clone();
            assert!(
                !e.failed(&out, Failure::Refused, plus(t0, n)),
                "an alert is never given up"
            );
            fx = e.tick(plus(t0, n));
            assert_eq!(fx.publish.len(), 1, "still owed after {n} refusals");
        }
        let o = &fx.publish[0];
        assert_eq!(o.message, "Details in Alertmanager");
        assert_eq!(o.title, "UnitFehlgeschlagen");
        assert!(o.button.is_some(), "the minimal text keeps its button");
        // A delivery clears the count: the next step is written in full.
        send_all(&mut e, &fx, plus(t0, 20));
        let fx = e.tick(plus(t0, 900));
        assert_ne!(fx.publish[0].message, "Details in Alertmanager");
    }

    #[test]
    fn a_button_expires_after_button_valid_secs_and_a_new_step_brings_a_new_one() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let fx = e.on_webhook(&w, t0);
        let body = fx.publish[0].button.clone().unwrap();
        send_all(&mut e, &fx, t0);
        let valid = e.config().button_valid_secs as i64;
        assert_eq!(e.on_ack(&body, plus(t0, valid + 1)), AckOutcome::Rejected);
        let fx = e.tick(plus(t0, valid + 1));
        let fresh = fx.publish[0].button.clone().unwrap();
        assert!(matches!(
            e.on_ack(&fresh, plus(t0, valid + 2)),
            AckOutcome::Accepted(_)
        ));
    }

    #[test]
    fn a_button_signed_before_a_key_rotation_still_acknowledges() {
        let old = Secret::from("old-key-old-key-old-key-old-key-".to_string());
        let mut e = Engine::new(
            State::default(),
            Config::from_toml(MINIMAL).unwrap(),
            Secret::from(KEY.to_string()),
        )
        .unwrap()
        .with_previous_ack_key(Some(Secret::from(old.expose().to_string())));
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let fx = e.on_webhook(&w, t0);
        send_all(&mut e, &fx, t0);
        let id = fx.publish[0].id.clone().unwrap();
        let before_rotation = crate::mac::button_body(&old, &id, plus(t0, 3600));
        assert_eq!(e.on_ack(&before_rotation, t0), AckOutcome::Accepted(id));
    }

    #[test]
    fn a_failed_send_is_retried_on_the_next_tick() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let fx = e.on_webhook(&w, t0);
        assert_eq!(fx.publish.len(), 1);
        // no confirm: ntfy refused
        assert_eq!(e.tick(plus(t0, 15)).publish.len(), 1);
    }

    #[test]
    fn forged_and_unknown_acknowledgements_do_nothing() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let fx = e.on_webhook(&w, t0);
        send_all(&mut e, &fx, t0);
        assert_eq!(
            e.on_ack("a1.0123456789abcdef.00000000000000000000000000000000", t0),
            AckOutcome::Rejected
        );
        let stranger = crate::mac::button_body(
            &Secret::from(KEY.to_string()),
            &InstanceId::parse("fedcba9876543210").unwrap(),
            plus(t0, 60),
        );
        assert!(matches!(e.on_ack(&stranger, t0), AckOutcome::Unknown(_)));
        assert_eq!(
            e.tick(plus(t0, 900)).publish[0].kind,
            Kind::Step(1, false),
            "still escalating"
        );
    }

    #[test]
    fn reconciliation_resolves_what_alertmanager_no_longer_lists() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let fx = e.on_webhook(&w, t0);
        send_all(&mut e, &fx, t0);
        let empty: Vec<GettableAlert> = serde_json::from_str(&recorded("api-empty.json")).unwrap();
        let fx = e.on_reconcile(&empty, plus(t0, 30), plus(t0, 31));
        assert_eq!(fx.publish.len(), 1);
        assert_eq!(fx.publish[0].kind, Kind::Resolved);
        assert_eq!(fx.publish[0].title, "Resolved: UnitFehlgeschlagen");
        send_all(&mut e, &fx, plus(t0, 31));
        assert_eq!(e.state().instances.len(), 0);
    }

    #[test]
    fn an_answer_taken_before_insist_learned_of_an_instance_does_not_resolve_it() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let fx = e.on_webhook(&w, plus(t0, 10));
        send_all(&mut e, &fx, plus(t0, 10));
        let empty: Vec<GettableAlert> = serde_json::from_str(&recorded("api-empty.json")).unwrap();
        let fx = e.on_reconcile(&empty, plus(t0, 9), plus(t0, 11));
        assert!(fx.publish.is_empty());
        assert_eq!(e.open_instances(), 1);
    }

    #[test]
    fn reconciliation_picks_up_what_the_webhook_missed_and_respects_silences() {
        let mut e = engine();
        let active: Vec<GettableAlert> =
            serde_json::from_str(&recorded("api-active.json")).unwrap();
        let t0 = active[0].starts_at;
        let fx = e.on_reconcile(&active, t0, t0);
        assert!(fx.publish.iter().any(|o| o.kind == Kind::Step(0, false)));

        let mut e = engine();
        let suppressed: Vec<GettableAlert> =
            serde_json::from_str(&recorded("api-suppressed.json")).unwrap();
        let fx = e.on_reconcile(&suppressed, t0, plus(t0, 3600));
        let silenced: Vec<_> = suppressed
            .iter()
            .filter(|a| a.status.state == "suppressed")
            .map(|a| InstanceId::of(&a.fingerprint, a.starts_at))
            .collect();
        assert!(
            !fx.publish.is_empty(),
            "the non-suppressed alerts in the same answer must still publish"
        );
        assert!(fx
            .publish
            .iter()
            .all(|o| !silenced.contains(o.id.as_ref().unwrap())));
        let suppressed_id = &silenced[0];
        let instance = e
            .state()
            .instances
            .get(suppressed_id)
            .expect("the silenced alert is still tracked, just not published");
        assert!(
            instance.suppressed,
            "reconciliation must mark it suppressed"
        );
    }

    #[test]
    fn a_silence_that_ends_lets_the_instance_escalate_by_its_age() {
        let mut e = engine();
        let suppressed: Vec<GettableAlert> =
            serde_json::from_str(&recorded("api-suppressed.json")).unwrap();
        let critical = suppressed
            .iter()
            .find(|a| a.status.state == "suppressed")
            .unwrap();
        let t0 = critical.starts_at;
        let id = InstanceId::of(&critical.fingerprint, t0);
        e.on_reconcile(&suppressed, t0, t0);
        assert!(e.state().instances.get(&id).unwrap().suppressed);

        let active: Vec<GettableAlert> =
            serde_json::from_str(&recorded("api-active.json")).unwrap();
        let fx = e.on_reconcile(&active, plus(t0, 3600), plus(t0, 3600));
        let step = fx
            .publish
            .iter()
            .find(|o| o.id.as_ref() == Some(&id))
            .expect("the unsilenced alert escalates");
        assert_eq!(
            step.kind,
            Kind::Step(2, false),
            "it climbs straight to the step its age deserves, not step 0"
        );
    }

    #[test]
    fn an_alert_without_a_ladder_is_sent_once_quietly_and_not_kept() {
        let mut e = engine();
        // constructed from the recorded single alert: severity info has no ladder
        let mut v: serde_json::Value =
            serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
        v["alerts"][0]["labels"]["severity"] = "info".into();
        let w: WebhookMessage = serde_json::from_value(v).unwrap();
        let fx = e.on_webhook(&w, w.alerts[0].starts_at);
        assert_eq!(fx.publish.len(), 1);
        assert_eq!(
            (fx.publish[0].kind.clone(), fx.publish[0].priority),
            (Kind::Once, 2)
        );
        assert!(fx.publish[0].button.is_none());
        assert!(e.state().instances.is_empty());
    }

    #[test]
    fn insists_own_alerts_are_ignored() {
        let mut e = engine();
        let mut v: serde_json::Value =
            serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
        v["alerts"][0]["labels"][OWN_LABEL] = "unacknowledged".into();
        let w: WebhookMessage = serde_json::from_value(v).unwrap();
        assert!(e.on_webhook(&w, w.alerts[0].starts_at).publish.is_empty());
        assert!(e.state().instances.is_empty());
    }

    #[test]
    fn a_probe_is_marked_and_uses_the_probe_ladder() {
        let mut e = engine();
        let mut v: serde_json::Value =
            serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
        v["alerts"][0]["labels"]["insist_probe"] = "ja".into();
        let w: WebhookMessage = serde_json::from_value(v).unwrap();
        let t0 = w.alerts[0].starts_at;
        let fx = e.on_webhook(&w, t0);
        assert!(fx.publish[0].probe);
        send_all(&mut e, &fx, t0);
        assert_eq!(e.tick(plus(t0, 60)).publish[0].priority, 5);
    }

    #[test]
    fn an_instance_resolved_before_anything_was_sent_disappears_silently() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let _unsent = e.on_webhook(&w, t0);
        let resolved: WebhookMessage = {
            let mut v: serde_json::Value =
                serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
            v["alerts"][0]["status"] = "resolved".into();
            serde_json::from_value(v).unwrap()
        };
        let fx = e.on_webhook(&resolved, plus(t0, 5));
        assert!(fx.publish.is_empty());
        assert!(e.state().instances.is_empty());
    }

    #[test]
    fn an_instance_with_a_future_ends_at_survives_reconciliation_until_it_passes() {
        let mut e = engine();
        let mut active: Vec<GettableAlert> =
            serde_json::from_str(&recorded("api-active.json")).unwrap();
        let t0 = active[0].starts_at;
        let future_ends = plus(t0, 7200);
        active[0].ends_at = future_ends;

        let fx = e.on_reconcile(&active, t0, t0);
        assert_eq!(fx.publish[0].kind, Kind::Step(0, false));
        send_all(&mut e, &fx, t0);

        // Alertmanager's storage is memory-only: an empty answer after a
        // restart must not read as an all-clear before endsAt.
        let empty: Vec<GettableAlert> = serde_json::from_str(&recorded("api-empty.json")).unwrap();
        let fx = e.on_reconcile(&empty, plus(t0, 30), plus(t0, 60));
        assert!(fx.publish.is_empty(), "no false Resolved before endsAt");
        assert_eq!(e.open_instances(), 1);

        // Listed again before endsAt: same instance, ladder not restarted.
        let fx = e.on_reconcile(&active, plus(t0, 3600), plus(t0, 3600));
        assert!(
            !fx.publish.iter().any(|o| o.kind == Kind::Step(0, false)),
            "the ladder must not restart: {fx:?}"
        );
        assert_eq!(e.open_instances(), 1);
        send_all(&mut e, &fx, plus(t0, 3600));

        // Once endsAt has passed and the alert is still absent, it resolves.
        let fx = e.on_reconcile(&empty, plus(future_ends, 30), plus(future_ends, 60));
        assert_eq!(fx.publish[0].kind, Kind::Resolved);
    }

    #[test]
    fn an_api_alert_for_a_receiver_insist_does_not_own_is_skipped() {
        let mut e = engine();
        let mut active: Vec<GettableAlert> =
            serde_json::from_str(&recorded("api-active.json")).unwrap();
        active[0].receivers = vec![crate::alertmanager::Receiver {
            name: "irgendein-mailversand".into(),
        }];
        let t0 = active[0].starts_at;
        let fx = e.on_reconcile(&active, t0, t0);
        assert!(fx.publish.is_empty());
        assert!(e.state().instances.is_empty());
    }

    #[test]
    fn a_firing_webhook_for_a_known_instance_clears_suppressed() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let id = InstanceId::of(&w.alerts[0].fingerprint, t0);
        e.on_webhook(&w, t0);
        e.state_mut().instances.get_mut(&id).unwrap().suppressed = true;

        // Alertmanager never webhooks a silenced or inhibited alert, so its
        // arrival is itself proof the instance is no longer suppressed.
        e.on_webhook(&w, plus(t0, 5));
        assert!(!e.state().instances.get(&id).unwrap().suppressed);
    }

    #[test]
    fn an_acknowledged_instance_can_still_resolve_with_the_same_id() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let fx = e.on_webhook(&w, t0);
        let id = fx.publish[0].id.clone().unwrap();
        let body = fx.publish[0].button.clone().unwrap();
        send_all(&mut e, &fx, t0);
        assert!(matches!(
            e.on_ack(&body, plus(t0, 10)),
            AckOutcome::Accepted(_)
        ));
        let fx = e.tick(plus(t0, 11));
        assert_eq!(fx.publish[0].kind, Kind::Acknowledged);
        send_all(&mut e, &fx, plus(t0, 11));

        let resolved: WebhookMessage = {
            let mut v: serde_json::Value =
                serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
            v["alerts"][0]["status"] = "resolved".into();
            serde_json::from_value(v).unwrap()
        };
        let fx = e.on_webhook(&resolved, plus(t0, 20));
        assert_eq!(fx.publish.len(), 1);
        assert_eq!(fx.publish[0].kind, Kind::Resolved);
        assert_eq!(fx.publish[0].id, Some(id));
    }

    #[test]
    fn an_instance_acknowledged_before_any_step_was_confirmed_still_resolves_visibly() {
        // state_mut reaches a shape the public flow does not normally
        // produce (an ack without a prior confirmed step), to pin down the
        // defensive rule: a pressed button proves a notification exists, so
        // Resolved must replace it rather than vanish silently.
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        e.on_webhook(&w, t0);
        let id = InstanceId::of(&w.alerts[0].fingerprint, t0);
        e.state_mut()
            .instances
            .get_mut(&id)
            .unwrap()
            .acknowledged_at = Some(t0);

        let resolved: WebhookMessage = {
            let mut v: serde_json::Value =
                serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
            v["alerts"][0]["status"] = "resolved".into();
            serde_json::from_value(v).unwrap()
        };
        let fx = e.on_webhook(&resolved, plus(t0, 5));
        assert_eq!(fx.publish.len(), 1);
        assert_eq!(fx.publish[0].kind, Kind::Resolved);
        assert_eq!(fx.publish[0].id, Some(id));
    }

    #[test]
    fn no_unacknowledged_mail_is_raised_once_a_human_has_acknowledged() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        let fx = e.on_webhook(&w, t0);
        let body = fx.publish[0].button.clone().unwrap();
        send_all(&mut e, &fx, t0);
        assert!(matches!(
            e.on_ack(&body, plus(t0, 10)),
            AckOutcome::Accepted(_)
        ));
        let fx = e.tick(plus(t0, 20));
        send_all(&mut e, &fx, plus(t0, 20));

        // Long past the raise_unacknowledged step (3600 s): must not raise.
        let fx = e.tick(plus(t0, 4000));
        assert!(
            fx.raise.is_empty(),
            "acknowledged instances never raise the unacknowledged mail"
        );
    }

    #[test]
    fn a_warning_seen_at_night_is_repeated_loudly_when_the_night_ends() {
        // Owner decision 2026-09-13: the quiet night substitute is deferred,
        // not skipped — the loud notice it stood in for is still owed the
        // moment the night ends, even though this instance is far too young
        // (2 h old) for step 1 to be due on its own.
        let mut e = engine();
        let mut v: serde_json::Value =
            serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
        v["alerts"][0]["labels"]["severity"] = "warning".into();
        v["alerts"][0]["startsAt"] = "2026-09-12T03:00:00Z".into(); // 05:00 local (CEST)
        let w: WebhookMessage = serde_json::from_value(v).unwrap();
        let t0 = w.alerts[0].starts_at;

        let fx = e.on_webhook(&w, t0);
        assert_eq!(fx.publish.len(), 1);
        assert_eq!(
            (fx.publish[0].kind.clone(), fx.publish[0].priority),
            (Kind::Step(0, true), 2),
            "the first notice at night goes out quietly"
        );
        let id = fx.publish[0].id.clone().unwrap();
        send_all(&mut e, &fx, t0);
        assert!(
            e.state().instances.get(&id).unwrap().last_quiet,
            "confirm records that the send was the quiet substitute"
        );

        assert!(
            e.tick(plus(t0, 7140)).publish.is_empty(),
            "06:59 local: still night, no second quiet send"
        );

        let fx = e.tick(plus(t0, 7200));
        assert_eq!(fx.publish.len(), 1);
        assert_eq!(
            (fx.publish[0].kind.clone(), fx.publish[0].priority),
            (Kind::Step(0, false), 3),
            "07:00 local: the deferred loud notice goes out, still step 0"
        );
        send_all(&mut e, &fx, plus(t0, 7200));
        assert!(
            !e.state().instances.get(&id).unwrap().last_quiet,
            "confirm clears the quiet flag on the loud send"
        );

        assert!(
            e.tick(plus(t0, 7500)).publish.is_empty(),
            "07:05 local: the loud repeat was just sent, nothing new due"
        );
    }

    // Measured live 2026-09-14: Alertmanager 0.31.1 sets startsAt = endsAt
    // for an alert posted with endsAt but no startsAt. A producer doing that
    // (or one whose clock runs ahead) must not switch escalation off.
    const DAY_AND_A_BIT: i64 = 26 * 3600;

    #[test]
    fn a_start_in_the_future_escalates_from_when_insist_first_saw_it() {
        let mut e = engine();
        let t0 = single().alerts[0].starts_at;
        let w = single_starting_at(plus(t0, DAY_AND_A_BIT));

        let fx = e.on_webhook(&w, t0);
        assert_eq!(fx.publish.len(), 1);
        assert_eq!(fx.publish[0].kind, Kind::Step(0, false));
        send_all(&mut e, &fx, t0);

        assert!(e.tick(plus(t0, 899)).publish.is_empty());
        let fx = e.tick(plus(t0, 900));
        assert_eq!(
            fx.publish
                .iter()
                .map(|o| (o.kind.clone(), o.priority))
                .collect::<Vec<_>>(),
            vec![(Kind::Step(1, false), 5)],
            "the critical ladder's step 1 is due 900 s after insist first saw it"
        );
        send_all(&mut e, &fx, plus(t0, 900));

        let fx = e.tick(plus(t0, 3600));
        assert_eq!(fx.publish[0].kind, Kind::Step(2, false));
        assert_eq!(fx.raise.len(), 1);
        assert_eq!(
            fx.raise[0].minutes, 60,
            "minutes unacknowledged count from first sight, never negative"
        );
    }

    #[test]
    fn the_message_of_a_future_start_names_when_insist_first_saw_it() {
        let mut e = engine();
        let t0 = single().alerts[0].starts_at; // 14:36 in Berlin
        let fx = e.on_webhook(&single_starting_at(plus(t0, DAY_AND_A_BIT)), t0);
        assert_eq!(
            fx.publish[0].message,
            "Unit lan6-set.service auf server ist rot (since 14:36)"
        );
    }

    #[test]
    fn a_start_before_insist_learned_of_it_still_counts_from_the_start() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;
        // Learned of ten minutes late (an outage, a restart).
        let fx = e.on_webhook(&w, plus(t0, 600));
        assert_eq!(fx.publish[0].kind, Kind::Step(0, false));
        assert_eq!(
            fx.publish[0].message,
            "Unit lan6-set.service auf server ist rot (since 14:36)"
        );
        send_all(&mut e, &fx, plus(t0, 600));
        assert!(e.tick(plus(t0, 899)).publish.is_empty());
        assert_eq!(
            e.tick(plus(t0, 900)).publish[0].kind,
            Kind::Step(1, false),
            "step 1 is due by the alert's own age, not by first sight"
        );
    }

    #[test]
    fn a_future_start_is_reported_once_per_instance() {
        let mut e = engine();
        let t0 = single().alerts[0].starts_at;
        let future = plus(t0, DAY_AND_A_BIT);
        let w = single_starting_at(future);

        let fx = e.on_webhook(&w, t0);
        assert_eq!(
            fx.future_starts,
            vec![FutureStart {
                id: InstanceId::of(&w.alerts[0].fingerprint, future),
                alertname: "UnitFehlgeschlagen".into(),
                starts_at: future,
                first_seen: t0,
            }]
        );
        send_all(&mut e, &fx, t0);

        // The same instance again, by every path that can meet it.
        assert!(e.on_webhook(&w, plus(t0, 60)).future_starts.is_empty());
        assert!(e.tick(plus(t0, 120)).future_starts.is_empty());
        let mut active: Vec<GettableAlert> =
            serde_json::from_str(&recorded("api-active.json")).unwrap();
        let listed = active
            .iter_mut()
            .find(|a| a.fingerprint == w.alerts[0].fingerprint)
            .unwrap();
        listed.starts_at = future;
        let fx = e.on_reconcile(&active, plus(t0, 180), plus(t0, 180));
        assert!(fx.future_starts.is_empty(), "{:?}", fx.future_starts);
        assert_eq!(e.open_instances(), active.len());
    }

    #[test]
    fn a_start_at_most_a_minute_ahead_is_not_reported() {
        let mut e = engine();
        let t0 = single().alerts[0].starts_at;
        assert!(e.on_webhook(&single(), t0).future_starts.is_empty());
        assert!(e
            .on_webhook(&single_starting_at(plus(t0, 60)), t0)
            .future_starts
            .is_empty());
        assert_eq!(
            e.on_webhook(&single_starting_at(plus(t0, 61)), t0)
                .future_starts
                .len(),
            1
        );
    }
}
