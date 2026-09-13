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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Secrets;

    #[test]
    fn parses_key_value_lines_and_ignores_comments() {
        let s =
            Secrets::parse("# comment\nNTFY_TOPIC=alarme-xyz\nNTFY_TOKEN = tk_abc\n\n").unwrap();
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
        assert!(Secrets::parse("NTFY_TOPIC=\nNTFY_TOKEN=tk\n").is_err());
    }
}
