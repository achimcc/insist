# Source

Recorded 2026-09-13 by `fixtures/record.sh` against
Alertmanager 0.31.1 from nixpkgs (the version the homeserver runs), with
a local webhook recorder. Nothing in this directory was written by hand.

## Measured

- **startsAt after re-posting the same label set 3 s later:**
  first `2026-09-13T12:36:04.000Z`, second `2026-09-13T12:36:04.000Z` →
  **kept**.
- **Precision:** webhook `2026-09-13T12:36:04Z`, API `2026-09-13T12:36:04.000Z`.
- `api-empty.json` was taken after posting `endsAt` in the past for every
  alert: resolved alerts are absent from `GET /api/v2/alerts`.

## Checked against the running instance

Checked against the running instance (`root@192.168.178.184`, `10.0.20.12:9093`,
2026-09-13): `fingerprint` is 16 hex characters there too, and `startsAt`
carries the same millisecond precision as `api-active.json`
(e.g. `2026-09-13T12:05:13.634Z`). No label values or alert counts from the
running server are recorded here or anywhere else in this repository.
