//! The credential file: KEY=VALUE lines, handed in by systemd.
use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug)]
pub struct Secrets {
    /// The alarm topic. Its name is itself a secret: anyone who knows it and
    /// holds any token with access can read the alerts.
    pub topic: Secret,
    /// The publishing token of the ntfy user `alarm`.
    pub token: Secret,
    /// The token inside every Acknowledge button. ntfy lets it write to the
    /// acknowledgement topic and nothing else.
    pub button_token: Secret,
    pub ack_key: Secret,
    /// The external dead man's switch. Its URL is its credential.
    pub watchdog_url: Secret,
    /// The bearer token `POST /` and `POST /watchdog` demand. Without it a
    /// forged `resolved` deletes an instance and holds the escalation at
    /// stage 0 forever, and any body at all pings the dead man's switch
    /// (audit B41).
    pub webhook_token: Secret,
}

impl Secrets {
    pub fn read(path: &Path) -> Result<Secrets> {
        // The path is not a secret; its content is, and it is never echoed.
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Secrets::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Secrets> {
        let mut values = BTreeMap::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                values.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let mut take = |key: &str| -> Result<Secret> {
            match values.remove(key) {
                Some(v) if !v.is_empty() => Ok(Secret::from(v)),
                _ => bail!("the credential file lacks {key}"),
            }
        };
        Ok(Secrets {
            topic: take("NTFY_TOPIC")?,
            token: take("NTFY_TOKEN")?,
            button_token: take("NTFY_BUTTON_TOKEN")?,
            ack_key: take("ACK_HMAC_KEY")?,
            watchdog_url: take("WATCHDOG_URL")?,
            webhook_token: take("WEBHOOK_TOKEN")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Secrets;

    const FULL: &str = "NTFY_TOPIC=alarme-xyz\nNTFY_TOKEN=tk_abc\nNTFY_BUTTON_TOKEN=tk_knopf\nACK_HMAC_KEY=00ff\nWATCHDOG_URL=https://hc.example/ping/geheim\nWEBHOOK_TOKEN=tk_hook\n";

    #[test]
    fn parses_key_value_lines_and_ignores_comments() {
        let s = Secrets::parse(&format!("# comment\n{FULL}")).unwrap();
        assert_eq!(s.topic.expose(), "alarme-xyz");
        assert_eq!(s.token.expose(), "tk_abc");
    }

    #[test]
    fn a_missing_key_names_the_key_but_never_a_value() {
        let err = Secrets::parse("NTFY_TOPIC=geheimes-topic\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("NTFY_TOKEN"), "{err}");
        assert!(!err.contains("geheimes-topic"), "{err}");
    }

    #[test]
    fn an_empty_value_counts_as_missing() {
        assert!(Secrets::parse(&FULL.replace("NTFY_TOPIC=alarme-xyz", "NTFY_TOPIC=")).is_err());
    }

    #[test]
    fn parses_all_six_keys() {
        let s = Secrets::parse(FULL).unwrap();
        assert_eq!(s.button_token.expose(), "tk_knopf");
        assert_eq!(s.ack_key.expose(), "00ff");
        assert_eq!(s.watchdog_url.expose(), "https://hc.example/ping/geheim");
        assert_eq!(s.webhook_token.expose(), "tk_hook");
    }

    #[test]
    fn each_key_is_required_and_named_without_values() {
        for key in [
            "NTFY_TOPIC",
            "NTFY_TOKEN",
            "NTFY_BUTTON_TOKEN",
            "ACK_HMAC_KEY",
            "WATCHDOG_URL",
            "WEBHOOK_TOKEN",
        ] {
            let without: String = FULL
                .lines()
                .filter(|l| !l.starts_with(&format!("{key}=")))
                .map(|l| format!("{l}\n"))
                .collect();
            let err = Secrets::parse(&without).unwrap_err().to_string();
            assert!(err.contains(key), "{err}");
            assert!(!err.contains("geheim") && !err.contains("tk_"), "{err}");
        }
    }
}
