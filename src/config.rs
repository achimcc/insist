//! The TOML configuration file: listen address, ntfy URL, credential file
//! path, timezone, the probe match and the notification texts.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub ntfy_url: String,
    pub alertmanager_url: String,
    pub state_file: PathBuf,
    pub secrets_file: PathBuf,
    pub timezone: String,
    pub ack_topic_suffix: String,
    pub reconcile_secs: u64,
    pub tick_secs: u64,
    pub watchdog_max_age_secs: u64,
    pub unacknowledged_alertname: String,
    /// The Alertmanager receivers whose webhook points at insist. `GET
    /// /api/v2/alerts` also returns alerts insist never received a webhook
    /// for (routed to a mail-only receiver, say); reconciliation skips any
    /// alert whose receivers do not intersect this list.
    pub receivers: Vec<String>,
    pub probe: ProbeMatch,
    pub night: crate::ladder::Night,
    pub ladders: std::collections::BTreeMap<String, crate::ladder::Ladder>,
    #[serde(default)]
    pub texts: Texts,
    /// While notifications have been failing (`insist_publish_pending` > 0)
    /// for longer than this, the watchdog ping is no longer forwarded, so
    /// the external dead man's switch raises the alarm. `InsistSendetNicht`
    /// says the same through Alertmanager — and a silence mutes it along
    /// with everything else (audit 3, B78). 0 turns this off. Default 900,
    /// the `for:` of that rule.
    #[serde(default = "default_withhold")]
    pub watchdog_withhold_pending_secs: u64,
    /// How long an Acknowledge button stays valid after it was sent. Every
    /// ladder step sends a fresh one, so this only has to outlast the
    /// longest gap between two sends of one instance; `validate` demands
    /// that gap plus a day (for the night). Default seven days.
    #[serde(default = "default_button_valid")]
    pub button_valid_secs: u64,
    /// What a credential shorter than 32 bytes (`WEBHOOK_TOKEN`,
    /// `ACK_HMAC_KEY`) does at start: `warn` (the default) logs it and
    /// shows it as `insist_short_secrets`, `refuse` stops insist.
    #[serde(default)]
    pub short_secrets: ShortSecrets,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShortSecrets {
    #[default]
    Warn,
    Refuse,
}

fn default_withhold() -> u64 {
    900
}

fn default_button_valid() -> u64 {
    7 * 86400
}

/// Alerts carrying `label = value` go to `<topic><topic_suffix>` instead of
/// the alarm topic, so a probe never rings a phone.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeMatch {
    pub label: String,
    pub value: String,
    pub topic_suffix: String,
}

/// The few fixed words in a notification. Defaults are English; a
/// deployment sets its own language.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Texts {
    pub resolved: String,
    pub since: String,
    pub acknowledge: String,
    pub acknowledged: String,
    /// `{alertname}` and `{minutes}` are replaced.
    pub unacknowledged: String,
    /// Title of the notice about an Alertmanager silence; `{matchers}` is
    /// replaced.
    pub silenced: String,
    /// Its message; `{until}`, `{by}` and `{comment}` are replaced. It
    /// must END with `{comment}`: the comment is the one part written by
    /// whoever set the silence, and it goes last, in quotes, so it cannot
    /// pose as the rest of the notice.
    pub silence_detail: String,
    /// The message of a notification sent as a minimal text, after ntfy
    /// refused the full one a few times.
    pub details_elsewhere: String,
}

impl Default for Texts {
    fn default() -> Self {
        Texts {
            resolved: "Resolved".into(),
            since: "since".into(),
            acknowledge: "Acknowledge".into(),
            acknowledged: "Acknowledged".into(),
            unacknowledged: "{alertname} unacknowledged for {minutes} min".into(),
            silenced: "Alerts silenced: {matchers}".into(),
            silence_detail: "until {until} · {by}: {comment}".into(),
            details_elsewhere: "Details in Alertmanager".into(),
        }
    }
}

/// `https://`, or `http://` only to an address no name lookup can move
/// (audit 3, B121): loopback, `localhost`, or a private or unique-local IP
/// literal. Through a NAME, whoever answers DNS for it would get the
/// token, the topic and every button in clear text.
fn check_url(field: &str, raw: &str) -> Result<()> {
    use anyhow::bail;
    use std::net::IpAddr;
    let url = reqwest::Url::parse(raw).with_context(|| format!("{field} is not a URL"))?;
    match url.scheme() {
        "https" => return Ok(()),
        "http" => {}
        other => bail!("{field}: scheme {other} is neither https nor http"),
    }
    // host_str keeps an IPv6 literal in brackets.
    let host = url.host_str().unwrap_or("");
    let fixed = match host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        Ok(IpAddr::V4(a)) => a.is_loopback() || a.is_private(),
        Ok(IpAddr::V6(a)) => a.is_loopback() || a.is_unique_local(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    };
    if !fixed {
        bail!("{field} must be https:// (plain http only to loopback or a private IP address)");
    }
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Config::from_toml(&raw).with_context(|| format!("in {}", path.display()))
    }

    pub fn from_toml(raw: &str) -> Result<Config> {
        let config: Config = toml::from_str(raw)?;
        // Fail at start, not at the first alert at night.
        config.tz()?;
        config.validate()?;
        Ok(config)
    }

    pub fn tz(&self) -> Result<jiff::tz::TimeZone> {
        jiff::tz::TimeZone::get(&self.timezone)
            .with_context(|| format!("unknown timezone {}", self.timezone))
    }

    fn validate(&self) -> Result<()> {
        use anyhow::bail;
        if self.receivers.is_empty() {
            bail!("receivers must not be empty");
        }
        for name in ["critical", "warning", "probe"] {
            if !self.ladders.contains_key(name) {
                bail!("the required ladder {name} is missing");
            }
        }
        for (name, ladder) in &self.ladders {
            let Some(first) = ladder.steps.first() else {
                bail!("ladder {name} has no steps")
            };
            if first.after_secs != 0 {
                bail!("ladder {name} must start at after_secs = 0");
            }
            for pair in ladder.steps.windows(2) {
                if pair[1].after_secs <= pair[0].after_secs {
                    bail!("ladder {name}: after_secs must climb");
                }
            }
            for step in &ladder.steps {
                if !(1..=5).contains(&step.priority) {
                    bail!("ladder {name}: priority must be 1..=5");
                }
                if step.repeat_secs == Some(0) {
                    bail!("ladder {name}: repeat_secs = 0 would send every tick");
                }
            }
        }
        if !(0..24).contains(&self.night.start_hour) || !(0..24).contains(&self.night.end_hour) {
            bail!("night hours must be 0..=23");
        }
        if self.reconcile_secs == 0 || self.tick_secs == 0 {
            bail!("reconcile_secs and tick_secs must be positive");
        }
        if !self.texts.silence_detail.ends_with("{comment}") {
            bail!("texts.silence_detail must end with {{comment}}");
        }
        let longest_gap = self
            .ladders
            .values()
            .flat_map(|l| {
                let steps = l
                    .steps
                    .windows(2)
                    .map(|p| p[1].after_secs - p[0].after_secs);
                steps.chain(l.steps.iter().filter_map(|s| s.repeat_secs))
            })
            .max()
            .unwrap_or(0);
        if self.button_valid_secs < longest_gap + 86400 {
            bail!(
                "button_valid_secs must be at least the longest gap between two sends ({longest_gap} s) plus a day"
            );
        }
        check_url("ntfy_url", &self.ntfy_url)?;
        check_url("alertmanager_url", &self.alertmanager_url)?;
        Ok(())
    }

    /// Which ladder an alert climbs, by name. A probe wins over severity; a
    /// missing severity is a warning. None: no ladder, send once, keep no state.
    pub fn ladder_for(
        &self,
        labels: &crate::alertmanager::Labels,
    ) -> Option<(String, &crate::ladder::Ladder)> {
        let name = if labels.get(&self.probe.label) == Some(&self.probe.value) {
            "probe".to_string()
        } else {
            labels
                .get("severity")
                .cloned()
                .unwrap_or_else(|| "warning".to_string())
        };
        self.ladders.get(&name).map(|l| (name, l))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::Config;

    pub(crate) const MINIMAL: &str = include_str!("../fixtures/constructed-config.toml");

    fn labels(pairs: &[(&str, &str)]) -> crate::alertmanager::Labels {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn loads_a_minimal_file_with_default_texts() {
        let c = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(c.listen.port(), 9099);
        assert_eq!(c.texts.resolved, "Resolved");
        assert!(c.tz().is_ok());
    }

    #[test]
    fn texts_can_be_overridden() {
        let c =
            Config::from_toml(&format!("{MINIMAL}\n[texts]\nresolved = \"Erledigt\"\n")).unwrap();
        assert_eq!(c.texts.resolved, "Erledigt");
        assert_eq!(c.texts.since, "since");
    }

    #[test]
    fn an_unknown_timezone_is_an_error_at_load_time() {
        let bad = MINIMAL.replace("Europe/Berlin", "Mars/Olympus");
        assert!(Config::from_toml(&bad).is_err());
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        assert!(Config::from_toml(&format!("{MINIMAL}\nlsiten = 1\n")).is_err());
    }

    #[test]
    fn the_ladder_follows_severity_probe_wins_and_missing_severity_is_warning() {
        let c = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(
            c.ladder_for(&labels(&[("severity", "critical")]))
                .unwrap()
                .0,
            "critical"
        );
        assert_eq!(c.ladder_for(&labels(&[])).unwrap().0, "warning");
        assert_eq!(
            c.ladder_for(&labels(&[("severity", "critical"), ("insist_probe", "ja")]))
                .unwrap()
                .0,
            "probe"
        );
        assert!(c.ladder_for(&labels(&[("severity", "info")])).is_none());
    }

    #[test]
    fn a_ladder_must_start_at_zero_and_climb() {
        let bad = MINIMAL.replace(
            "{ after_secs = 0, priority = 3 }",
            "{ after_secs = 60, priority = 3 }",
        );
        assert!(Config::from_toml(&bad).is_err());
        let bad = MINIMAL.replace(
            "{ after_secs = 14400, repeat_secs = 43200, priority = 4 }",
            "{ after_secs = 0, priority = 4 }",
        );
        assert!(Config::from_toml(&bad).is_err());
        let bad = MINIMAL.replace(
            "{ after_secs = 60, repeat_secs = 60, priority = 5 }",
            "{ after_secs = 60, repeat_secs = 60, priority = 9 }",
        );
        assert!(Config::from_toml(&bad).is_err());
    }

    #[test]
    fn a_zero_repeat_is_an_error() {
        let bad = MINIMAL.replace("repeat_secs = 60,", "repeat_secs = 0,");
        assert!(Config::from_toml(&bad).is_err());
    }

    #[test]
    fn each_of_the_three_required_ladders_is_mandatory() {
        let removals: &[(&str, &str)] = &[
            (
                "critical",
                "[ladders.critical]\nsteps = [\n  { after_secs = 0, priority = 4 },\n  { after_secs = 900, repeat_secs = 900, priority = 5 },\n  { after_secs = 3600, repeat_secs = 300, priority = 5, raise_unacknowledged = true },\n]\n\n",
            ),
            (
                "warning",
                "[ladders.warning]\nquiet_at_night = true\nsteps = [\n  { after_secs = 0, priority = 3 },\n  { after_secs = 14400, repeat_secs = 43200, priority = 4 },\n]\n\n",
            ),
            (
                "probe",
                "[ladders.probe]\nsteps = [\n  { after_secs = 0, priority = 4 },\n  { after_secs = 60, repeat_secs = 60, priority = 5 },\n]\n",
            ),
        ];
        for (name, block) in removals {
            assert!(
                MINIMAL.contains(block),
                "fixture no longer contains the {name} block verbatim"
            );
            let bad = MINIMAL.replace(block, "");
            assert_ne!(&bad, MINIMAL, "removing {name} did not change the fixture");
            let err = Config::from_toml(&bad).unwrap_err().to_string();
            assert!(err.contains(name), "{err}");
        }
    }

    #[test]
    fn plain_http_only_to_an_address_no_lookup_can_move() {
        let with = |field: &str, url: &str| {
            let line = MINIMAL
                .lines()
                .find(|l| l.starts_with(&format!("{field} =")))
                .unwrap();
            Config::from_toml(&MINIMAL.replace(line, &format!("{field} = \"{url}\"")))
        };
        for field in ["ntfy_url", "alertmanager_url"] {
            for ok in [
                "https://ntfy.example",
                "http://127.0.0.1:2586",
                "http://localhost:9093",
                "http://10.0.20.12:9093",
                "http://192.168.178.184",
                "http://[::1]:9093",
                "http://[fd00:20::12]:9093",
            ] {
                assert!(with(field, ok).is_ok(), "{field} = {ok}");
            }
            for bad in [
                "http://ntfy.example",
                "http://8.8.8.8",
                "http://[2001:db8::1]",
                "ftp://10.0.0.1",
                "ntfy.example",
            ] {
                let err = with(field, bad).unwrap_err();
                assert!(
                    format!("{err:#}").contains(field),
                    "{field} = {bad}: {err:#}"
                );
            }
        }
    }

    #[test]
    fn the_silence_comment_must_come_last() {
        let bad = format!("{MINIMAL}\n[texts]\nsilence_detail = \"{{comment}} until {{until}}\"\n");
        assert!(Config::from_toml(&bad).is_err());
        let ok = format!(
            "{MINIMAL}\n[texts]\nsilence_detail = \"bis {{until}} · {{by}}: {{comment}}\"\n"
        );
        assert!(Config::from_toml(&ok).is_ok());
    }

    #[test]
    fn new_settings_default_and_the_button_outlives_the_longest_gap() {
        let c = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(c.watchdog_withhold_pending_secs, 900);
        assert_eq!(c.button_valid_secs, 7 * 86400);
        assert_eq!(c.short_secrets, super::ShortSecrets::Warn);
        // The warning ladder repeats every 43200 s: a day more is the floor.
        let edge = |secs: u64| {
            let raw = MINIMAL.replace(
                "watchdog_max_age_secs = 300",
                &format!("watchdog_max_age_secs = 300\nbutton_valid_secs = {secs}"),
            );
            Config::from_toml(&raw)
        };
        assert!(edge(43200 + 86400).is_ok());
        assert!(edge(43200 + 86399).is_err());
        let refuse = MINIMAL.replace(
            "watchdog_max_age_secs = 300",
            "watchdog_max_age_secs = 300\nshort_secrets = \"refuse\"",
        );
        assert_eq!(
            Config::from_toml(&refuse).unwrap().short_secrets,
            super::ShortSecrets::Refuse
        );
    }

    #[test]
    fn empty_receivers_is_an_error() {
        assert!(
            MINIMAL.contains("receivers = [\"rec\"]"),
            "fixture no longer names a receiver"
        );
        let bad = MINIMAL.replace("receivers = [\"rec\"]", "receivers = []");
        assert_ne!(
            &bad, MINIMAL,
            "removing the receiver did not change the fixture"
        );
        let err = Config::from_toml(&bad).unwrap_err().to_string();
        assert!(err.contains("receiver"), "{err}");
    }
}
