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

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Step(usize),
    Acknowledged,
    Resolved,
    Once,
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

#[derive(Debug, Clone, PartialEq)]
pub struct Unacknowledged {
    pub id: InstanceId,
    pub alertname: String,
    pub summary: String,
    pub minutes: i64,
}

#[derive(Debug, Default, PartialEq)]
pub struct Effects {
    pub publish: Vec<Outgoing>,
    pub raise: Vec<Unacknowledged>,
}

impl Effects {
    fn extend(&mut self, other: Effects) {
        self.publish.extend(other.publish);
        self.raise.extend(other.raise);
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
}

impl Engine {
    pub fn new(state: State, config: Config, ack_key: Secret) -> anyhow::Result<Engine> {
        let tz = config.tz()?;
        Ok(Engine {
            state,
            config,
            tz,
            ack_key,
        })
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

    fn upsert(
        &mut self,
        fingerprint: &str,
        starts_at: Timestamp,
        labels: &Labels,
        annotations: &Labels,
        now: Timestamp,
    ) -> Option<InstanceId> {
        let (ladder, _) = self.config.ladder_for(labels)?;
        let id = InstanceId::of(fingerprint, starts_at);
        let probe = self.is_probe(labels);
        self.state
            .instances
            .entry(id.clone())
            .or_insert_with(|| Instance {
                fingerprint: fingerprint.to_string(),
                starts_at,
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
            });
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
                    effects.publish.push(Outgoing {
                        id: None,
                        kind: Kind::Once,
                        probe: self.is_probe(&alert.labels),
                        title: render::alertname(&alert.labels),
                        message: render::summary(&alert.labels, &alert.annotations),
                        priority: 2,
                        tags: vec!["information_source".into()],
                        button: None,
                    });
                }
                continue;
            }
            if resolved {
                let id = InstanceId::of(&alert.fingerprint, alert.starts_at);
                if let Some(instance) = self.state.instances.get_mut(&id) {
                    instance.resolved_at.get_or_insert(now);
                }
            } else {
                self.upsert(
                    &alert.fingerprint,
                    alert.starts_at,
                    &alert.labels,
                    &alert.annotations,
                    now,
                );
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
        let mut present = BTreeSet::new();
        for alert in alerts {
            if Self::is_own(&alert.labels) {
                continue;
            }
            if let Some(id) = self.upsert(
                &alert.fingerprint,
                alert.starts_at,
                &alert.labels,
                &alert.annotations,
                now,
            ) {
                if let Some(instance) = self.state.instances.get_mut(&id) {
                    instance.suppressed = alert.status.state == "suppressed";
                }
                present.insert(id);
            }
        }
        for (id, instance) in self.state.instances.iter_mut() {
            if instance.resolved_at.is_none()
                && instance.first_seen <= fetched_at
                && !present.contains(id)
            {
                instance.resolved_at = Some(now);
            }
        }
        self.tick(now)
    }

    pub fn tick(&mut self, now: Timestamp) -> Effects {
        // Resolved before anything went out: nothing to take back on a phone.
        self.state
            .instances
            .retain(|_, i| !(i.resolved_at.is_some() && i.last_step.is_none()));

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
            };
            if let Some(due) = ladder::due(
                ladder,
                instance.starts_at,
                &progress,
                now,
                &self.config.night,
                &self.tz,
            ) {
                effects.publish.push(self.step(id, instance, &due));
                if due.raise_unacknowledged && !instance.unacknowledged_raised {
                    effects.raise.push(Unacknowledged {
                        id: id.clone(),
                        alertname: instance.alertname.clone(),
                        summary: instance.summary.clone(),
                        minutes: (now.as_second() - instance.starts_at.as_second()) / 60,
                    });
                }
            }
        }
        effects
    }

    fn step(&self, id: &InstanceId, i: &Instance, due: &Due) -> Outgoing {
        let loud = i.ladder == "critical" || i.probe;
        Outgoing {
            id: Some(id.clone()),
            kind: Kind::Step(due.step),
            probe: i.probe,
            title: i.alertname.clone(),
            message: format!(
                "{} ({} {})",
                i.summary,
                self.config.texts.since,
                render::clock(i.starts_at, &self.tz)
            ),
            priority: due.priority,
            tags: vec![if loud { "rotating_light" } else { "warning" }.into()],
            button: Some(mac::button_body(&self.ack_key, id)),
        }
    }

    fn acknowledged(&self, id: &InstanceId, i: &Instance, at: Timestamp) -> Outgoing {
        Outgoing {
            id: Some(id.clone()),
            kind: Kind::Acknowledged,
            probe: i.probe,
            title: format!(
                "{} {}: {}",
                self.config.texts.acknowledged,
                render::clock(at, &self.tz),
                i.alertname
            ),
            message: i.summary.clone(),
            priority: 2,
            tags: vec!["ballot_box_with_check".into()],
            button: None,
        }
    }

    fn resolved(&self, id: &InstanceId, i: &Instance) -> Outgoing {
        Outgoing {
            id: Some(id.clone()),
            kind: Kind::Resolved,
            probe: i.probe,
            title: format!("{}: {}", self.config.texts.resolved, i.alertname),
            message: i.summary.clone(),
            priority: 2,
            tags: vec!["white_check_mark".into()],
            button: None,
        }
    }

    pub fn confirm(&mut self, out: &Outgoing, now: Timestamp) {
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
            Kind::Step(step) => {
                if let Some(i) = self.state.instances.get_mut(id) {
                    i.last_step = Some(step);
                    i.last_sent = Some(now);
                }
            }
            Kind::Once => {}
        }
    }

    pub fn confirm_raise(&mut self, id: &InstanceId) {
        if let Some(i) = self.state.instances.get_mut(id) {
            i.unacknowledged_raised = true;
        }
    }

    pub fn on_ack(&mut self, body: &str, now: Timestamp) -> AckOutcome {
        let Some(id) = mac::verify(&self.ack_key, body) else {
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
    fn send_all(e: &mut Engine, fx: &Effects, now: Timestamp) {
        for o in &fx.publish {
            e.confirm(o, now);
        }
        for r in &fx.raise {
            e.confirm_raise(&r.id);
        }
    }

    #[test]
    fn a_recorded_critical_alert_climbs_the_ladder_until_acknowledged() {
        let mut e = engine();
        let w = single();
        let t0 = w.alerts[0].starts_at;

        let fx = e.on_webhook(&w, t0);
        assert_eq!(fx.publish.len(), 1);
        let first = &fx.publish[0];
        assert_eq!(first.kind, Kind::Step(0));
        assert_eq!(first.priority, 4);
        assert_eq!(first.title, "UnitFehlgeschlagen");
        let body = first.button.clone().expect("a button");
        assert_eq!(
            crate::mac::verify(&Secret::from(KEY.to_string()), &body),
            first.id
        );
        send_all(&mut e, &fx, t0);

        assert!(e.tick(plus(t0, 60)).publish.is_empty());

        let fx = e.tick(plus(t0, 900));
        assert_eq!(
            (fx.publish[0].kind.clone(), fx.publish[0].priority),
            (Kind::Step(1), 5)
        );
        send_all(&mut e, &fx, plus(t0, 900));

        let fx = e.tick(plus(t0, 3600));
        assert_eq!(fx.publish[0].kind, Kind::Step(2));
        assert_eq!(fx.raise.len(), 1);
        assert_eq!(fx.raise[0].minutes, 60);
        send_all(&mut e, &fx, plus(t0, 3600));

        let fx = e.tick(plus(t0, 3900));
        assert_eq!(fx.publish[0].kind, Kind::Step(2));
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
        );
        assert!(matches!(e.on_ack(&stranger, t0), AckOutcome::Unknown(_)));
        assert_eq!(
            e.tick(plus(t0, 900)).publish[0].kind,
            Kind::Step(1),
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
        assert!(fx.publish.iter().any(|o| o.kind == Kind::Step(0)));

        let mut e = engine();
        let suppressed: Vec<GettableAlert> =
            serde_json::from_str(&recorded("api-suppressed.json")).unwrap();
        let fx = e.on_reconcile(&suppressed, t0, plus(t0, 3600));
        let silenced: Vec<_> = suppressed
            .iter()
            .filter(|a| a.status.state == "suppressed")
            .map(|a| InstanceId::of(&a.fingerprint, a.starts_at))
            .collect();
        assert!(fx
            .publish
            .iter()
            .all(|o| !silenced.contains(o.id.as_ref().unwrap())));
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
}
