# Source

Recorded 2026-09-13 by `fixtures/record.sh` against ntfy
2.26.0 from nixpkgs, configured like the homeserver (deny-all, users
`alarm` and `knopf`, tokens from `ntfy token generate`). Tokens were
generated for the run and died with it. The Authorization header inside the
published action is the literal `REDACTED`.

- `publish-response.json`: answer to a JSON publish with title, tags,
  sequence_id and an http action.
- `poll-after-replace.ndjson`: the topic after a second publish with the
  same sequence_id. ntfy stores updates append-only; both messages carry
  the same sequence_id; the replacement happens in the client
  (docs.ntfy.sh/publish, "Updating + deleting notifications").
- `ack-poll.ndjson`, `ack-since.ndjson`: two button bodies, then a poll
  with since=<first id>.
- `stream.ndjson`: 12 s of the /json stream, keepalive-interval 5s.
- `forbidden.txt`: knopf writing to the alarm topic.
