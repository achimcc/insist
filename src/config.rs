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
    pub secrets_file: PathBuf,
    pub timezone: String,
    pub probe: ProbeMatch,
    #[serde(default)]
    pub texts: Texts,
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
}

impl Default for Texts {
    fn default() -> Self {
        Texts {
            resolved: "Resolved".into(),
            since: "since".into(),
            acknowledge: "Acknowledge".into(),
            acknowledged: "Acknowledged".into(),
            unacknowledged: "{alertname} unacknowledged for {minutes} min".into(),
        }
    }
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
        Ok(config)
    }

    pub fn tz(&self) -> Result<jiff::tz::TimeZone> {
        jiff::tz::TimeZone::get(&self.timezone)
            .with_context(|| format!("unknown timezone {}", self.timezone))
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    const MINIMAL: &str = r#"
listen = "127.0.0.1:9099"
ntfy_url = "https://ntfy.example"
secrets_file = "/run/credentials/insist.service/insist-env"
timezone = "Europe/Berlin"

[probe]
label = "insist_probe"
value = "ja"
topic_suffix = "-selbstprobe"
"#;

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
}
