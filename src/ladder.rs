//! When an open instance is due for its next notification. Pure: no clock,
//! no IO — everything it needs is an argument, so a table can test it.
use jiff::{tz::TimeZone, Timestamp};
use serde::Deserialize;

/// What a step at night sends when the ladder is quiet at night and nothing
/// has been sent yet: visible, but silent on the phone.
pub const QUIET_PRIORITY: u8 = 2;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub after_secs: u64,
    #[serde(default)]
    pub repeat_secs: Option<u64>,
    pub priority: u8,
    #[serde(default)]
    pub raise_unacknowledged: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ladder {
    pub steps: Vec<Step>,
    #[serde(default)]
    pub quiet_at_night: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Night {
    pub start_hour: i8,
    pub end_hour: i8,
}

impl Night {
    pub fn contains(&self, at: Timestamp, tz: &TimeZone) -> bool {
        let hour = at.to_zoned(tz.clone()).hour();
        if self.start_hour <= self.end_hour {
            hour >= self.start_hour && hour < self.end_hour
        } else {
            hour >= self.start_hour || hour < self.end_hour
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    pub last_step: Option<usize>,
    pub last_sent: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Due {
    pub step: usize,
    pub priority: u8,
    pub raise_unacknowledged: bool,
}

fn seconds_between(earlier: Timestamp, later: Timestamp) -> u64 {
    later.as_second().saturating_sub(earlier.as_second()).max(0) as u64
}

pub fn due(
    ladder: &Ladder,
    starts_at: Timestamp,
    progress: &Progress,
    now: Timestamp,
    night: &Night,
    tz: &TimeZone,
) -> Option<Due> {
    let age = seconds_between(starts_at, now);
    let current = ladder.steps.iter().rposition(|s| s.after_secs <= age)?;
    let step = &ladder.steps[current];

    let entering = progress.last_step.is_none_or(|last| current > last);
    let repeating = !entering
        && match (step.repeat_secs, progress.last_sent) {
            (Some(every), Some(sent)) => seconds_between(sent, now) >= every,
            _ => false,
        };
    if !entering && !repeating {
        return None;
    }

    if ladder.quiet_at_night && night.contains(now, tz) {
        // Deferred, not skipped: the condition still holds at the end of the
        // night, so the step goes out then. Only the very first notice is
        // sent at once — quietly, and as step 0, so the loud step it would
        // have been still counts as "entering" in the morning.
        return progress.last_step.is_none().then_some(Due {
            step: 0,
            priority: QUIET_PRIORITY.min(ladder.steps[0].priority),
            raise_unacknowledged: false,
        });
    }

    Some(Due {
        step: current,
        priority: step.priority,
        raise_unacknowledged: step.raise_unacknowledged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }
    fn berlin() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("Europe/Berlin").unwrap()
    }
    fn night() -> Night {
        Night {
            start_hour: 22,
            end_hour: 7,
        }
    }
    fn critical() -> Ladder {
        Ladder {
            quiet_at_night: false,
            steps: vec![
                Step {
                    after_secs: 0,
                    repeat_secs: None,
                    priority: 4,
                    raise_unacknowledged: false,
                },
                Step {
                    after_secs: 900,
                    repeat_secs: Some(900),
                    priority: 5,
                    raise_unacknowledged: false,
                },
                Step {
                    after_secs: 3600,
                    repeat_secs: Some(300),
                    priority: 5,
                    raise_unacknowledged: true,
                },
            ],
        }
    }
    fn warning() -> Ladder {
        Ladder {
            quiet_at_night: true,
            steps: vec![
                Step {
                    after_secs: 0,
                    repeat_secs: None,
                    priority: 3,
                    raise_unacknowledged: false,
                },
                Step {
                    after_secs: 14400,
                    repeat_secs: Some(43200),
                    priority: 4,
                    raise_unacknowledged: false,
                },
            ],
        }
    }
    fn p(last_step: Option<usize>, last_sent: Option<&str>) -> Progress {
        Progress {
            last_step,
            last_sent: last_sent.map(ts),
        }
    }

    // 2026-09-11 is CEST (UTC+2). 15:55Z = 17:55 local.
    const START: &str = "2026-09-11T15:55:00Z";

    #[test]
    #[allow(clippy::type_complexity)]
    fn table_for_critical() {
        let l = critical();
        let cases: &[(&str, Progress, Option<(usize, u8, bool)>)] = &[
            ("2026-09-11T15:55:00Z", p(None, None), Some((0, 4, false))),
            ("2026-09-11T16:05:00Z", p(Some(0), Some(START)), None),
            (
                "2026-09-11T16:10:00Z",
                p(Some(0), Some(START)),
                Some((1, 5, false)),
            ),
            (
                "2026-09-11T16:20:00Z",
                p(Some(1), Some("2026-09-11T16:10:00Z")),
                None,
            ),
            (
                "2026-09-11T16:25:00Z",
                p(Some(1), Some("2026-09-11T16:10:00Z")),
                Some((1, 5, false)),
            ),
            (
                "2026-09-11T16:55:00Z",
                p(Some(1), Some("2026-09-11T16:40:00Z")),
                Some((2, 5, true)),
            ),
            (
                "2026-09-11T16:59:00Z",
                p(Some(2), Some("2026-09-11T16:55:00Z")),
                None,
            ),
            (
                "2026-09-11T17:00:00Z",
                p(Some(2), Some("2026-09-11T16:55:00Z")),
                Some((2, 5, true)),
            ),
            // Learned late (after an outage): straight to the step its age deserves.
            ("2026-09-11T18:00:00Z", p(None, None), Some((2, 5, true))),
            // critical ignores the night: 23:30 local
            (
                "2026-09-11T21:30:00Z",
                p(Some(2), Some("2026-09-11T21:20:00Z")),
                Some((2, 5, true)),
            ),
        ];
        for (now, progress, expected) in cases {
            let got = due(&l, ts(START), progress, ts(now), &night(), &berlin())
                .map(|d| (d.step, d.priority, d.raise_unacknowledged));
            assert_eq!(&got, expected, "now {now}");
        }
    }

    #[test]
    fn a_warning_that_starts_at_night_arrives_quiet_and_reminds_at_seven() {
        let l = warning();
        let start = ts("2026-09-11T23:00:00Z"); // 01:00 local
        let first = due(&l, start, &p(None, None), start, &night(), &berlin()).unwrap();
        assert_eq!((first.step, first.priority), (0, QUIET_PRIORITY));
        // 4 h later is 05:00 local: the reminder is due but deferred
        assert_eq!(
            due(
                &l,
                start,
                &p(Some(0), Some("2026-09-11T23:00:00Z")),
                ts("2026-09-12T03:00:00Z"),
                &night(),
                &berlin()
            ),
            None
        );
        // 06:59 local: still night
        assert_eq!(
            due(
                &l,
                start,
                &p(Some(0), Some("2026-09-11T23:00:00Z")),
                ts("2026-09-12T04:59:00Z"),
                &night(),
                &berlin()
            ),
            None
        );
        // 07:00 local: the deferred step goes out, loud
        let at_seven = due(
            &l,
            start,
            &p(Some(0), Some("2026-09-11T23:00:00Z")),
            ts("2026-09-12T05:00:00Z"),
            &night(),
            &berlin(),
        )
        .unwrap();
        assert_eq!((at_seven.step, at_seven.priority), (1, 4));
    }

    #[test]
    fn a_warning_due_at_21_59_goes_out_and_one_due_at_22_00_waits() {
        let l = warning();
        let start = ts("2026-09-11T15:59:00Z"); // 17:59 local, step 1 due at 21:59
        let at_21_59 = due(
            &l,
            start,
            &p(Some(0), Some("2026-09-11T15:59:00Z")),
            ts("2026-09-11T19:59:00Z"),
            &night(),
            &berlin(),
        )
        .unwrap();
        assert_eq!((at_21_59.step, at_21_59.priority), (1, 4));
        let start = ts("2026-09-11T16:00:00Z"); // step 1 due at 22:00 local
        assert!(due(
            &l,
            start,
            &p(Some(0), Some("2026-09-11T16:00:00Z")),
            ts("2026-09-11T20:00:00Z"),
            &night(),
            &berlin()
        )
        .is_none());
    }

    #[test]
    fn night_follows_the_clock_change_on_2026_10_25() {
        let n = night();
        // After the switch at 01:00Z local time is UTC+1.
        assert!(n.contains(ts("2026-10-25T01:30:00Z"), &berlin())); // 02:30 CET
        assert!(n.contains(ts("2026-10-25T05:59:00Z"), &berlin())); // 06:59 CET
        assert!(!n.contains(ts("2026-10-25T06:00:00Z"), &berlin())); // 07:00 CET

        // The day before, UTC+2.
        assert!(n.contains(ts("2026-10-24T04:59:00Z"), &berlin())); // 06:59 CEST
        assert!(!n.contains(ts("2026-10-24T05:00:00Z"), &berlin())); // 07:00 CEST
        assert!(n.contains(ts("2026-10-24T20:00:00Z"), &berlin())); // 22:00 CEST
        assert!(!n.contains(ts("2026-10-24T19:59:00Z"), &berlin())); // 21:59 CEST
    }

    #[test]
    fn a_start_in_the_future_counts_as_age_zero() {
        let now = ts("2026-09-11T15:54:00Z");
        let d = due(
            &critical(),
            ts(START),
            &p(None, None),
            now,
            &night(),
            &berlin(),
        )
        .unwrap();
        assert_eq!(d.step, 0);
    }
}
