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
    ///
    /// Optional since 0.5.0: a deployment can ping the dead man's switch from
    /// a machine the alert path runs on top of, so that whoever takes over the
    /// machine insist runs on does not hold the one URL that reports it
    /// silent. Without it `POST /watchdog` answers 404 and forwards nothing.
    pub watchdog_url: Option<Secret>,
    /// The bearer token `POST /` and `POST /watchdog` demand. Without it a
    /// forged `resolved` deletes an instance and holds the escalation at
    /// stage 0 forever, and any body at all pings the dead man's switch
    /// (audit B41).
    pub webhook_token: Secret,
    /// Optional: the HMAC key before the last rotation. Buttons it signed
    /// keep working until the phone shows newer ones; remove it after the
    /// longest `button_valid_secs`.
    pub ack_key_previous: Option<Secret>,
}

/// Shorter than this, a credential a stranger must not guess is short.
/// Both are generated, never typed, so 32 bytes cost nothing.
pub const MIN_SECRET_BYTES: usize = 32;

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
        let watchdog_url = values
            .remove("WATCHDOG_URL")
            .filter(|v| !v.is_empty())
            .map(Secret::from);
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
            watchdog_url,
            webhook_token: take("WEBHOOK_TOKEN")?,
            ack_key_previous: take("ACK_HMAC_KEY_PREVIOUS").ok(),
        })
    }

    /// The names — never the values — of the credentials shorter than
    /// `MIN_SECRET_BYTES` (audit 3, B120): the two that stand between a
    /// stranger and a forged webhook or a forged button.
    pub fn short(&self) -> Vec<&'static str> {
        let mut short = Vec::new();
        let mut check = |name, s: Option<&Secret>| {
            if s.is_some_and(|s| s.expose().len() < MIN_SECRET_BYTES) {
                short.push(name);
            }
        };
        check("WEBHOOK_TOKEN", Some(&self.webhook_token));
        check("ACK_HMAC_KEY", Some(&self.ack_key));
        check("ACK_HMAC_KEY_PREVIOUS", self.ack_key_previous.as_ref());
        short
    }

    /// What `short_secrets` in the configuration says to do about them:
    /// an error naming them for `refuse`, the list for `warn`.
    pub fn check_length(&self, policy: crate::config::ShortSecrets) -> Result<Vec<&'static str>> {
        let short = self.short();
        if policy == crate::config::ShortSecrets::Refuse && !short.is_empty() {
            bail!(
                "shorter than {MIN_SECRET_BYTES} bytes: {} (short_secrets = \"refuse\")",
                short.join(", ")
            );
        }
        Ok(short)
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

    const LONG: &str = "NTFY_TOPIC=alarme-xyz\nNTFY_TOKEN=tk_abc\nNTFY_BUTTON_TOKEN=tk_knopf\nACK_HMAC_KEY=00ff00ff00ff00ff00ff00ff00ff00ff\nWATCHDOG_URL=https://hc.example/ping/geheim\nWEBHOOK_TOKEN=tk_hook_tk_hook_tk_hook_tk_hook_\n";

    #[test]
    fn short_credentials_are_named_and_refused_only_when_asked() {
        use crate::config::ShortSecrets;
        let long = Secrets::parse(LONG).unwrap();
        assert!(long.short().is_empty());
        assert!(long.check_length(ShortSecrets::Refuse).is_ok());

        let short = Secrets::parse(FULL).unwrap();
        assert_eq!(short.short(), vec!["WEBHOOK_TOKEN", "ACK_HMAC_KEY"]);
        assert_eq!(
            short.check_length(ShortSecrets::Warn).unwrap(),
            vec!["WEBHOOK_TOKEN", "ACK_HMAC_KEY"]
        );
        let err = short
            .check_length(ShortSecrets::Refuse)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("WEBHOOK_TOKEN") && err.contains("ACK_HMAC_KEY"),
            "{err}"
        );
        assert!(!err.contains("tk_hook") && !err.contains("00ff"), "{err}");

        let with_previous = Secrets::parse(&format!("{LONG}ACK_HMAC_KEY_PREVIOUS=abc\n")).unwrap();
        assert_eq!(with_previous.short(), vec!["ACK_HMAC_KEY_PREVIOUS"]);
        assert_eq!(with_previous.ack_key_previous.unwrap().expose(), "abc");
        assert!(Secrets::parse(LONG).unwrap().ack_key_previous.is_none());
    }

    #[test]
    fn parses_all_six_keys() {
        let s = Secrets::parse(FULL).unwrap();
        assert_eq!(s.button_token.expose(), "tk_knopf");
        assert_eq!(s.ack_key.expose(), "00ff");
        assert_eq!(
            s.watchdog_url.as_ref().unwrap().expose(),
            "https://hc.example/ping/geheim"
        );
        assert_eq!(s.webhook_token.expose(), "tk_hook");
    }

    #[test]
    fn each_key_is_required_and_named_without_values() {
        for key in [
            "NTFY_TOPIC",
            "NTFY_TOKEN",
            "NTFY_BUTTON_TOKEN",
            "ACK_HMAC_KEY",
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

    /// Since 0.5.0 the dead man's switch may be pinged from elsewhere: a file
    /// without `WATCHDOG_URL`, or with an empty one, is complete.
    #[test]
    fn the_watchdog_url_is_optional_and_empty_means_absent() {
        let without: String = FULL
            .lines()
            .filter(|l| !l.starts_with("WATCHDOG_URL="))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(Secrets::parse(&without).unwrap().watchdog_url.is_none());
        let empty = FULL.replace(
            "WATCHDOG_URL=https://hc.example/ping/geheim",
            "WATCHDOG_URL=",
        );
        assert!(Secrets::parse(&empty).unwrap().watchdog_url.is_none());
    }
}
