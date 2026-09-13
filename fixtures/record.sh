#!/usr/bin/env bash
# Records real answers from the Alertmanager and ntfy versions the homeserver
# runs. Run inside `nix develop`. Writes into fixtures/<program>-<version>/.
set -euo pipefail
cd "$(dirname "$0")"

am_version=$(alertmanager --version 2>&1 | sed -n 's/^alertmanager, version \([0-9.]*\).*/\1/p')
ntfy_version=$(readlink -f "$(command -v ntfy)" | sed -n 's/.*ntfy-sh-\([0-9.]*\)\/.*/\1/p')
[ "$am_version" = "0.31.1" ] || { echo "alertmanager is $am_version, expected 0.31.1" >&2; exit 1; }
[ "$ntfy_version" = "2.26.0" ] || { echo "ntfy is $ntfy_version, expected 2.26.0" >&2; exit 1; }

work=$(mktemp -d)
cleanup() { kill $(jobs -p) 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT

AM=alertmanager-0.31.1; NT=ntfy-2.26.0
mkdir -p "$AM" "$NT"

# ---------------------------------------------------------------- Alertmanager
python3 recorder.py 19199 "$work" &
cat > "$work/am.yml" <<'YAML'
route:
  receiver: rec
  group_by: [alertname]
  group_wait: 1s
  group_interval: 2s
  repeat_interval: 1h
receivers:
  - name: rec
    webhook_configs:
      - url: http://127.0.0.1:19199/
        send_resolved: true
YAML
alertmanager --config.file="$work/am.yml" --storage.path="$work/am" \
  --web.listen-address=127.0.0.1:19093 --cluster.listen-address="" >"$work/am.log" 2>&1 &
api=http://127.0.0.1:19093/api/v2
for _ in $(seq 50); do curl -sf "$api/status" >/dev/null && break; sleep 0.2; done

post() { curl -sf -X POST -H 'Content-Type: application/json' --data "$1" "$api/alerts"; }
wait_for_webhooks() { # $1 = expected count
  for _ in $(seq 100); do [ "$(ls "$work"/raw-webhook-*.json 2>/dev/null | wc -l)" -ge "$1" ] && return; sleep 0.1; done
  echo "timed out waiting for webhook $1" >&2; exit 1
}
later=$(date -u -d '+10 minutes' +%Y-%m-%dT%H:%M:%SZ)

# 1. one firing alert, like UnitFehlgeschlagen on 2026-09-11
post "[{\"labels\":{\"alertname\":\"UnitFehlgeschlagen\",\"severity\":\"critical\",\"name\":\"lan6-set.service\",\"instance\":\"server\"},\"annotations\":{\"summary\":\"Unit lan6-set.service auf server ist rot\"},\"endsAt\":\"$later\"}]"
wait_for_webhooks 1; cp "$work/raw-webhook-01.json" "$AM/webhook-firing-single.json"
curl -sf "$api/alerts" | jq . > "$AM/api-active.json"

# 2. the same label set posted again three seconds later
first_starts=$(jq -r '.[0].startsAt' "$AM/api-active.json")
sleep 3
post "[{\"labels\":{\"alertname\":\"UnitFehlgeschlagen\",\"severity\":\"critical\",\"name\":\"lan6-set.service\",\"instance\":\"server\"},\"annotations\":{\"summary\":\"Unit lan6-set.service auf server ist rot\"},\"endsAt\":\"$later\"}]"
curl -sf "$api/alerts" | jq . > "$AM/api-reposted.json"
second_starts=$(jq -r '.[0].startsAt' "$AM/api-reposted.json")

# 3. a group of two, and an alert without severity
post "[{\"labels\":{\"alertname\":\"GastNichtAktiv\",\"severity\":\"warning\",\"gast\":\"kal-01\"},\"endsAt\":\"$later\"},{\"labels\":{\"alertname\":\"GastNichtAktiv\",\"severity\":\"warning\",\"gast\":\"rss-01\"},\"endsAt\":\"$later\"}]"
wait_for_webhooks 2; cp "$work/raw-webhook-02.json" "$AM/webhook-firing-group.json"
post "[{\"labels\":{\"alertname\":\"OhneSchwere\",\"instance\":\"server\"},\"annotations\":{\"summary\":\"Prüfung — ä\"},\"endsAt\":\"$later\"}]"
wait_for_webhooks 3; cp "$work/raw-webhook-03.json" "$AM/webhook-no-severity.json"

# 4. a silence on the first alert
now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
silence=$(curl -sf -X POST -H 'Content-Type: application/json' "$api/silences" --data "{\"matchers\":[{\"name\":\"alertname\",\"value\":\"UnitFehlgeschlagen\",\"isRegex\":false}],\"startsAt\":\"$now\",\"endsAt\":\"$later\",\"createdBy\":\"record.sh\",\"comment\":\"fixture\"}" | jq -r .silenceID)
sleep 1
curl -sf "$api/alerts" | jq . > "$AM/api-suppressed.json"
curl -sf -X DELETE "$api/silence/$silence"

# 5. resolve everything
past=$(date -u -d '-1 second' +%Y-%m-%dT%H:%M:%SZ)
for body in \
  '{"alertname":"UnitFehlgeschlagen","severity":"critical","name":"lan6-set.service","instance":"server"}' \
  '{"alertname":"GastNichtAktiv","severity":"warning","gast":"kal-01"}' \
  '{"alertname":"GastNichtAktiv","severity":"warning","gast":"rss-01"}' \
  '{"alertname":"OhneSchwere","instance":"server"}'; do
  post "[{\"labels\":$body,\"endsAt\":\"$past\"}]"
done
for _ in $(seq 100); do
  f=$(grep -l '"status":"resolved"' "$work"/raw-webhook-*.json 2>/dev/null | head -1 || true)
  [ -n "$f" ] && break; sleep 0.1
done
[ -n "$f" ] || { echo "no resolved webhook" >&2; exit 1; }
cp "$f" "$AM/webhook-resolved.json"
curl -sf "$api/alerts" | jq . > "$AM/api-empty.json"

webhook_starts=$(jq -r '.alerts[0].startsAt' "$AM/webhook-firing-single.json")
cat > "$AM/SOURCE.md" <<EOF
# Source

Recorded $(date -u +%Y-%m-%d) by \`fixtures/record.sh\` against
Alertmanager $am_version from nixpkgs (the version the homeserver runs), with
a local webhook recorder. Nothing in this directory was written by hand.

## Measured

- **startsAt after re-posting the same label set 3 s later:**
  first \`$first_starts\`, second \`$second_starts\` →
  $( [ "$first_starts" = "$second_starts" ] && echo "**kept**" || echo "**changed**" ).
- **Precision:** webhook \`$webhook_starts\`, API \`$first_starts\`.
- \`api-empty.json\` was taken after posting \`endsAt\` in the past for every
  alert: resolved alerts are $( [ "$(jq length "$AM/api-empty.json")" = 0 ] && echo "absent" || echo "STILL PRESENT" ) from \`GET /api/v2/alerts\`.
EOF

# ------------------------------------------------------------------------ ntfy
alarm_pw=$(head -c 18 /dev/urandom | base64); knopf_pw=$(head -c 18 /dev/urandom | base64)
alarm_hash=$(mkpasswd -m bcrypt -R 10 "$alarm_pw"); knopf_hash=$(mkpasswd -m bcrypt -R 10 "$knopf_pw")
alarm_token=$(ntfy token generate); knopf_token=$(ntfy token generate)
cat > "$work/ntfy.yml" <<YAML
base-url: "http://127.0.0.1:12586"
listen-http: "127.0.0.1:12586"
auth-file: "$work/user.db"
auth-default-access: "deny-all"
cache-file: "$work/cache.db"
keepalive-interval: "5s"
auth-users:
  - "alarm:$alarm_hash:user"
  - "knopf:$knopf_hash:user"
auth-tokens:
  - "alarm:$alarm_token"
  - "knopf:$knopf_token"
auth-access:
  - "alarm:alarmtopic*:rw"
  - "knopf:alarmtopic-quittung:write-only"
YAML
ntfy serve -c "$work/ntfy.yml" >"$work/ntfy.log" 2>&1 &
base=http://127.0.0.1:12586
for _ in $(seq 50); do curl -sf "$base/v1/health" >/dev/null && break; sleep 0.2; done

auth_alarm=(-H "Authorization: Bearer $alarm_token")
pub() { curl -sS "${auth_alarm[@]}" -H 'Content-Type: application/json' --data "$1" "$base/"; }

# stream in the background for 12 s: open, keepalive, messages
curl -sS -N --max-time 12 "${auth_alarm[@]}" "$base/alarmtopic/json" > "$NT/stream.ndjson" &
stream_pid=$!
sleep 1

pub '{"topic":"alarmtopic","title":"Prüfung — ä","message":"Unit lan6-set.service auf server ist rot","priority":4,"tags":["rotating_light"],"sequence_id":"0123456789abcdef","actions":[{"action":"http","label":"Quittieren","url":"http://127.0.0.1:12586/alarmtopic-quittung","method":"POST","headers":{"Authorization":"Bearer REDACTED"},"body":"a1.0123456789abcdef.00000000000000000000000000000000","clear":true}]}' \
  | jq . > "$NT/publish-response.json"
pub '{"topic":"alarmtopic","title":"UnitFehlgeschlagen","message":"zweite Stufe","priority":5,"sequence_id":"0123456789abcdef"}' > /dev/null
sleep 1
curl -sS "${auth_alarm[@]}" "$base/alarmtopic/json?poll=1" > "$NT/poll-after-replace.ndjson"

# the button, as the phone sends it: knopf token, plain body
curl -sS -X POST -H "Authorization: Bearer $knopf_token" --data 'a1.0123456789abcdef.00000000000000000000000000000000' "$base/alarmtopic-quittung" > /dev/null
curl -sS -X POST -H "Authorization: Bearer $knopf_token" --data 'a1.fedcba9876543210.11111111111111111111111111111111' "$base/alarmtopic-quittung" > /dev/null
curl -sS "${auth_alarm[@]}" "$base/alarmtopic-quittung/json?poll=1" > "$NT/ack-poll.ndjson"
first_ack=$(head -1 "$NT/ack-poll.ndjson" | jq -r .id)
curl -sS "${auth_alarm[@]}" "$base/alarmtopic-quittung/json?poll=1&since=$first_ack" > "$NT/ack-since.ndjson"

# knopf may not write to the alarm topic
code=$(curl -sS -o "$work/forbidden-body" -w '%{http_code}' -X POST -H "Authorization: Bearer $knopf_token" --data x "$base/alarmtopic")
{ echo "HTTP $code"; cat "$work/forbidden-body"; } > "$NT/forbidden.txt"

# Only wait for the backgrounded stream curl: alertmanager, ntfy serve and the
# recorder never exit on their own, so a bare `wait` here would hang forever.
wait "$stream_pid" 2>/dev/null || true

cat > "$NT/SOURCE.md" <<EOF
# Source

Recorded $(date -u +%Y-%m-%d) by \`fixtures/record.sh\` against ntfy
$ntfy_version from nixpkgs, configured like the homeserver (deny-all, users
\`alarm\` and \`knopf\`, tokens from \`ntfy token generate\`). Tokens were
generated for the run and died with it. The Authorization header inside the
published action is the literal \`REDACTED\`.

- \`publish-response.json\`: answer to a JSON publish with title, tags,
  sequence_id and an http action.
- \`poll-after-replace.ndjson\`: the topic after a second publish with the
  same sequence_id. ntfy stores updates append-only; both messages carry
  the same sequence_id; the replacement happens in the client
  (docs.ntfy.sh/publish, "Updating + deleting notifications").
- \`ack-poll.ndjson\`, \`ack-since.ndjson\`: two button bodies, then a poll
  with since=<first id>.
- \`stream.ndjson\`: 12 s of the /json stream, keepalive-interval 5s.
- \`forbidden.txt\`: knopf writing to the alarm topic.
EOF
echo "done"
