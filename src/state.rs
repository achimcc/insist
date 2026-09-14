//! What insist remembers across restarts. Alertmanager knows what is firing;
//! this file knows only what Alertmanager does not: which step went out when,
//! and whether a human acknowledged it.
use crate::instance::InstanceId;
use anyhow::{Context, Result};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    pub fingerprint: String,
    pub starts_at: Timestamp,
    /// The last real `endsAt` Alertmanager gave for it (its zero value maps
    /// to `None`, meaning no known deadline). Reconciliation only resolves
    /// an absent instance once this has passed — Alertmanager's storage is
    /// memory-only, and a restart must not read as an all-clear before then.
    /// `#[serde(default)]`: a state file written before this field existed
    /// must still load.
    #[serde(default)]
    pub ends_at: Option<Timestamp>,
    /// When insist first learned of it. Reconciliation must not resolve an
    /// instance it learned of after the API answer it is looking at was taken,
    /// and a `starts_at` in the future is aged from here instead (see
    /// `effective_start`).
    pub first_seen: Timestamp,
    pub alertname: String,
    pub ladder: String,
    pub summary: String,
    pub probe: bool,
    pub last_step: Option<usize>,
    pub last_sent: Option<Timestamp>,
    pub acknowledged_at: Option<Timestamp>,
    pub acknowledgement_sent: bool,
    pub resolved_at: Option<Timestamp>,
    pub suppressed: bool,
    pub unacknowledged_raised: bool,
    /// Whether the last confirmed send was the night substitute (see
    /// `ladder::Progress::quiet`) — so the next tick after the night ends
    /// knows the loud step it stood in for is still owed. `#[serde(default)]`:
    /// a state file written before this field existed must still load, as
    /// "not quiet" (the safe reading: nothing owed beyond the normal ladder).
    #[serde(default)]
    pub last_quiet: bool,
}

impl Instance {
    /// Where every age of this instance is measured from: its `startsAt`,
    /// unless insist saw it earlier than that. A `startsAt` in the future
    /// (a clock running ahead, or Alertmanager 0.31.1 filling an omitted
    /// `startsAt` with `endsAt`) would otherwise hold the age at zero and
    /// silence the ladder until that moment. For a correct producer
    /// `first_seen` is never earlier, so nothing changes.
    pub fn effective_start(&self) -> Timestamp {
        self.starts_at.min(self.first_seen)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub instances: BTreeMap<InstanceId, Instance>,
    /// The id of the last message read from the acknowledgement topic, so a
    /// restart resumes there instead of losing a press made during a deploy.
    pub ack_cursor: Option<String>,
}

pub struct Loaded {
    pub state: State,
    pub moved_corrupt_to: Option<PathBuf>,
}

impl State {
    pub fn load(path: &Path, now: Timestamp) -> Result<Loaded> {
        let raw = match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Loaded {
                    state: State::default(),
                    moved_corrupt_to: None,
                })
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
            Ok(raw) => raw,
        };
        match serde_json::from_str(&raw) {
            Ok(state) => Ok(Loaded {
                state,
                moved_corrupt_to: None,
            }),
            Err(_) => {
                // Loud, not lost: starting empty re-announces everything that
                // is firing. The broken file stays for a human to look at.
                let mut aside = path.as_os_str().to_owned();
                aside.push(format!(".corrupt-{}", now.as_second()));
                let aside = PathBuf::from(aside);
                std::fs::rename(path, &aside)
                    .with_context(|| format!("cannot move corrupt {} aside", path.display()))?;
                Ok(Loaded {
                    state: State::default(),
                    moved_corrupt_to: Some(aside),
                })
            }
        }
    }

    /// Temp file, fsync, rename, fsync the directory: a crash leaves the old
    /// file or the new one, never half of either.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .context("state file has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let temp = path.with_extension("json.new");
        {
            let mut file = std::fs::File::create(&temp)
                .with_context(|| format!("cannot write {}", temp.display()))?;
            std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, path)
            .with_context(|| format!("cannot rename onto {}", path.display()))?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        "2026-09-13T12:00:00Z".parse().unwrap()
    }

    fn sample() -> State {
        let mut s = State::default();
        s.instances.insert(
            InstanceId::parse("0123456789abcdef").unwrap(),
            Instance {
                fingerprint: "a1b2c3d4e5f60718".into(),
                starts_at: now(),
                ends_at: None,
                first_seen: now(),
                alertname: "UnitFehlgeschlagen".into(),
                ladder: "critical".into(),
                summary: "Unit lan6-set.service auf server ist rot".into(),
                probe: false,
                last_step: Some(1),
                last_sent: Some(now()),
                acknowledged_at: None,
                acknowledgement_sent: false,
                resolved_at: None,
                suppressed: false,
                unacknowledged_raised: false,
                last_quiet: false,
            },
        );
        s.ack_cursor = Some("hwQ2YpKdmg".into());
        s
    }

    #[test]
    fn the_effective_start_is_the_earlier_of_starts_at_and_first_seen() {
        let mut i = sample().instances.into_values().next().unwrap();
        // A producer with a clock ahead (or Alertmanager filling an omitted
        // startsAt with endsAt): escalation counts from when insist saw it.
        i.first_seen = now();
        i.starts_at = now() + jiff::SignedDuration::from_hours(26);
        assert_eq!(i.effective_start(), now());
        // Learned of late: the alert's own start still counts.
        i.starts_at = now() - jiff::SignedDuration::from_mins(10);
        assert_eq!(i.effective_start(), i.starts_at);
    }

    #[test]
    fn saves_and_loads_the_same_state_without_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        sample().save(&path).unwrap();
        let loaded = State::load(&path, now()).unwrap();
        assert_eq!(loaded.state, sample());
        assert!(loaded.moved_corrupt_to.is_none());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("state.json")]);
    }

    #[test]
    fn a_missing_file_is_an_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = State::load(&dir.path().join("state.json"), now()).unwrap();
        assert_eq!(loaded.state, State::default());
        assert!(loaded.moved_corrupt_to.is_none());
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ half a file").unwrap();
        let loaded = State::load(&path, now()).unwrap();
        assert_eq!(loaded.state, State::default());
        let moved = loaded.moved_corrupt_to.unwrap();
        assert_eq!(
            moved.file_name().unwrap().to_str().unwrap(),
            format!("state.json.corrupt-{}", now().as_second())
        );
        assert_eq!(std::fs::read_to_string(moved).unwrap(), "{ half a file");
        assert!(!path.exists());
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_an_empty_state() {
        // A directory where the file should be: reading fails for a reason
        // other than "not found", and starting empty would forget every
        // acknowledgement silently.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::create_dir(&path).unwrap();
        assert!(State::load(&path, now()).is_err());
    }

    #[test]
    fn an_old_state_file_without_ends_at_loads_with_none() {
        let mut with_field = sample();
        with_field
            .instances
            .get_mut(&InstanceId::parse("0123456789abcdef").unwrap())
            .unwrap()
            .ends_at = Some(now());
        let mut value = serde_json::to_value(&with_field).unwrap();
        let removed = value["instances"]["0123456789abcdef"]
            .as_object_mut()
            .unwrap()
            .remove("ends_at");
        assert!(
            removed.is_some(),
            "ends_at was not present in the serialised instance"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
        let loaded = State::load(&path, now()).unwrap();
        assert!(
            loaded.moved_corrupt_to.is_none(),
            "an old file must not be treated as corrupt"
        );
        let instance = loaded.state.instances.values().next().unwrap();
        assert_eq!(instance.ends_at, None);
    }

    #[test]
    fn an_old_state_file_without_last_quiet_loads_with_false() {
        let mut with_field = sample();
        with_field
            .instances
            .get_mut(&InstanceId::parse("0123456789abcdef").unwrap())
            .unwrap()
            .last_quiet = true;
        let mut value = serde_json::to_value(&with_field).unwrap();
        let removed = value["instances"]["0123456789abcdef"]
            .as_object_mut()
            .unwrap()
            .remove("last_quiet");
        assert!(
            removed.is_some(),
            "last_quiet was not present in the serialised instance"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
        let loaded = State::load(&path, now()).unwrap();
        assert!(
            loaded.moved_corrupt_to.is_none(),
            "an old file must not be treated as corrupt"
        );
        let instance = loaded.state.instances.values().next().unwrap();
        assert!(!instance.last_quiet);
    }
}
