#!/usr/bin/env bash
# Records GET /api/v2/silences from the Alertmanager version the homeserver
# runs: one active silence (with a regex matcher, like a "silence everything"
# would use), one pending (starts later) and, after expiring the first, the
# same list again. Run inside `nix develop`. Separate from record.sh so the
# alert fixtures, which tests pin to exact timestamps, stay as they are.
set -euo pipefail
cd "$(dirname "$0")"

am_version=$(alertmanager --version 2>&1 | sed -n 's/^alertmanager, version \([0-9.]*\).*/\1/p')
[ "$am_version" = "0.31.1" ] || { echo "alertmanager is $am_version, expected 0.31.1" >&2; exit 1; }

work=$(mktemp -d)
cleanup() { kill $(jobs -p) 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT

AM=alertmanager-0.31.1
mkdir -p "$AM"

cat > "$work/am.yml" <<'YAML'
route:
  receiver: none
receivers:
  - name: none
YAML
alertmanager --config.file="$work/am.yml" --storage.path="$work/am" \
  --web.listen-address=127.0.0.1:19094 --cluster.listen-address="" >"$work/am.log" 2>&1 &
api=http://127.0.0.1:19094/api/v2
for _ in $(seq 50); do curl -sf "$api/status" >/dev/null && break; sleep 0.2; done

now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
soon=$(date -u -d '+30 minutes' +%Y-%m-%dT%H:%M:%SZ)
later=$(date -u -d '+2 hours' +%Y-%m-%dT%H:%M:%SZ)

curl -sf "$api/silences" | jq . > "$AM/silences-empty.json"

active=$(curl -sf -X POST -H 'Content-Type: application/json' "$api/silences" --data "{\"matchers\":[{\"name\":\"alertname\",\"value\":\".+\",\"isRegex\":true,\"isEqual\":true}],\"startsAt\":\"$now\",\"endsAt\":\"$later\",\"createdBy\":\"record-silences.sh\",\"comment\":\"everything\"}" | jq -r .silenceID)
curl -sf -X POST -H 'Content-Type: application/json' "$api/silences" --data "{\"matchers\":[{\"name\":\"alertname\",\"value\":\"UnitFehlgeschlagen\",\"isRegex\":false,\"isEqual\":true},{\"name\":\"instance\",\"value\":\"server\",\"isRegex\":false,\"isEqual\":false}],\"startsAt\":\"$soon\",\"endsAt\":\"$later\",\"createdBy\":\"record-silences.sh\",\"comment\":\"later\"}" > /dev/null
sleep 1
curl -sf "$api/silences" | jq . > "$AM/silences-active-and-pending.json"

curl -sf -X DELETE "$api/silence/$active"
sleep 1
curl -sf "$api/silences" | jq . > "$AM/silences-after-expire.json"

cat > "$AM/SOURCE-silences.md" <<EOF
# Source (silences)

Recorded $(date -u +%Y-%m-%d) by \`fixtures/record-silences.sh\` against
Alertmanager $am_version from nixpkgs. Nothing here was written by hand.

- \`silences-empty.json\`: \`GET /api/v2/silences\` before any silence.
- \`silences-active-and-pending.json\`: one silence starting now with the
  regex matcher \`alertname=~".+"\`, one starting 30 minutes later with an
  equality and a negative matcher.
- \`silences-after-expire.json\`: the same list after \`DELETE
  /api/v2/silence/<id>\` on the active one — Alertmanager keeps it, with
  \`status.state\` \`expired\`.
EOF
echo done
