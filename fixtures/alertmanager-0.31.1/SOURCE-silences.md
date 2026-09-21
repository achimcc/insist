# Source (silences)

Recorded 2026-09-21 by `fixtures/record-silences.sh` against
Alertmanager 0.31.1 from nixpkgs. Nothing here was written by hand.

- `silences-empty.json`: `GET /api/v2/silences` before any silence.
- `silences-active-and-pending.json`: one silence starting now with the
  regex matcher `alertname=~".+"`, one starting 30 minutes later with an
  equality and a negative matcher.
- `silences-after-expire.json`: the same list after `DELETE
  /api/v2/silence/<id>` on the active one — Alertmanager keeps it, with
  `status.state` `expired`.
