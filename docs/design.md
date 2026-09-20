# insist — design

## Why

On 2026-09-11 a firewall rule silently fell away and stayed off for five
hours. A watcher noticed and mailed about it every fifteen minutes from
17:49 to 22:27 — 21 mails, every one delivered, none read — plus one urgent
push notification at 17:55 that also went unread.

The alerting was not missing coverage: it had seen the fault and said so,
repeatedly. What was missing was state. An alert has no memory of whether
anyone has looked at it, and nothing makes it louder when nobody has.

## Shape

Alertmanager is the truth about what is currently firing; insist keeps only
step, last send and acknowledgement for each alert instance. A webhook is
the trigger that gets the first notification out quickly; a periodic call
to the alerting API is reconciliation, the thing that decides what is
actually still open. What the API no longer lists is treated as resolved,
even if a resolving webhook was lost along the way — with one exception
(see Instances). Reconciliation only takes over alerts routed to the
receivers insist is configured to watch: the alerting API also lists alerts
routed to mail-only receivers, and those must never become instances insist
notifies about and waits on.

A webhook's group can carry several alerts at once (they share an
`alertname`, say); insist still sends one notification per instance, not
one per group, so that each alert's own escalation and acknowledgement stay
independent of whatever else happened to fire alongside it. The notification
itself is one JSON body to the push service, title included — not a title
carried in a transport header, which would otherwise have constrained every
alert title to a header-safe character set.

## Instances

An instance is identified by Alertmanager's fingerprint together with its
`startsAt`, truncated to millisecond precision. The truncation matters
because the webhook and the alerting API do not agree on precision: the
webhook carries `startsAt` to the nanosecond, the API to the millisecond
(see `fixtures/alertmanager-0.31.1/SOURCE.md`). Without truncating both to
the same precision, the webhook and the reconciliation pass would compute
two different ids for what is actually one firing, and reconciliation would
never find the id the webhook created.

If an alert fires again after being resolved, it gets a new `startsAt` and
therefore a new, unacknowledged instance — acknowledging one occurrence
never silences a later, unrelated one.

The alerting system keeps its firing alerts in memory only: after a
restart with nothing yet re-posted, a query for what is firing answers
with an empty list. Treating that emptiness as "everything is resolved"
would read a restart during a real outage as an all-clear. An instance
missing from the API is therefore only resolved once its own last known
end time has actually passed; until then, a gap in reconciliation is read
as a gap in reconciliation, not as good news. An instance that has never
had a real end time reported for it — one insist has only ever seen
through a webhook, which never carries a real one — has no deadline to
wait for, so its absence resolves it at once; the waiting is specifically
for instances whose known end time gives a reason not to conclude anything
yet.

Measured on the live deployment: when the Alertmanager/Prometheus host
itself restarts, Prometheus re-evaluates every rule and re-sends each
firing alert with a new `startsAt` (`FiredAt` is reset for every rule), not
just an empty list followed by a resend of the same one. insist therefore
sees a new instance under the identity above: the phone rings again, the
old instance is later read as resolved once its `endsAt` passes, an
acknowledgement made on it does not carry over to the new one, and the
unacknowledged-mail deadline restarts from zero. Carrying state per
fingerprint instead of per instance would avoid this and is a possible
future change.

## Ladders and night

Each severity has its own ladder: a table of steps, each with a delay since
the alert started, an optional repeat interval, and a priority to send at.
`critical` and `warning` both escalate until acknowledged, at different
paces — `critical` goes loud after fifteen minutes and, if still
unacknowledged after an hour, additionally raises its own alert (see
below); `warning` stays quiet for four hours, then repeats a loud reminder
every twelve. An alert with no `severity` label at all is treated as
`warning` and escalates on that ladder: a producer that forgot the label
must not make its alert quieter. Only a severity that is set but matches no
configured ladder (`info`, say) falls back to a single, stateless notice
with no button and no further escalation.

An instance the alerting system itself reports as suppressed (silenced or
inhibited) is never escalated by insist for as long as that lasts —
something a human has deliberately silenced elsewhere is not insist's to
keep pushing on. Once the suppression ends, the same instance simply
resumes escalating at whatever step its total age by then already earns,
not from the beginning.

A `warning` ladder can be marked quiet at night: if its very first notice
is due during the configured night hours, it goes out at a low priority
(2) instead of at the loud priority the step would otherwise carry — that
substitute is deferred, never skipped. When the night ends, the step the
alert's age deserves by then is sent at its own, full, loud priority,
whether or not the quiet substitute had already gone out. `critical` and
`probe` ladders ignore the night entirely and always escalate at full
volume.

A `critical` ladder can mark one step as the point past which an
unacknowledged instance is raised again — as its own single alert back
into the alerting system on a separate, mail-only route, rather than as
one more push notification among a mounting pile that eventually gets
ignored. That raised alert carries the label key `insist` (its value is
incidental); insist ignores every alert carrying that label key, regardless
of its value, so it can never end up watching, escalating, or raising an
alert about itself.

## Acknowledging

Every notification for a `critical`, `warning` or `probe` instance carries
an "Acknowledge" button, and each step for the same instance replaces the
previous notification on the phone rather than stacking a new one beside
it — the push service does this by matching on the instance id, which
doubles as that service's own identifier for the message. Pressing the
button posts a short signed body to a topic of its own —
`a1.<instance-id>.<mac>` — where `<mac>` authenticates the instance id
against a key only insist holds, compared in constant time so that timing
cannot leak anything about a correct value. The button's own token is
meant to be granted write access to that one topic only — never
permission to read alerts, or to post to the alert topic itself — though
enforcing that division is the operator's job, via the push service's own
access control, not insist's. A button token that leaks therefore cannot
forge an alert with a working button, and can only ever acknowledge the
one specific instance named in a body it captured — nothing more.

insist reads that topic as a stream, resuming after any disconnect or
restart from the id of the last line it read rather than from the
beginning of whatever the push service still has cached. A body that does
not verify is rejected, counted in a metric, and logged without ever
including the body itself — a rejected press's content might be anything a
holder of a leaked button token chose to send. A body that verifies but
names an instance already acknowledged or resolved, or one no longer known
at all, changes nothing; pressing the same button twice is harmless. Once
accepted, the notification for that instance is replaced by "Acknowledged
HH:MM" at a low priority and without a button, and nothing escalates for it
again. If the underlying alert resolves — acknowledged or not — its
notification is replaced once more, by "Resolved", also at a low priority.
Every acceptance, together with the stream position it was read at, is
written to the state file in the same atomic write as everything else, so
a restart resumes reading exactly where it left off.

## Failure behaviour

Every failure inside insist is designed to be either louder or visible
through some other, independent path — never silent. Mail is never routed
through insist: it goes directly from the alerting system, so a dead or
wedged insist never removes the plainest, most established alerting
channel there is. The external dead man's switch is pinged only after a
reconciliation pass has actually succeeded and is still fresh; a hung or
crashed insist starves it, and the switch itself raises the alarm from
outside.

| Situation | What happens |
|---|---|
| insist is dead or restart-looping | No push notifications, but mail keeps working. Its watchdog ping stops, so the external dead man's switch raises the alarm from outside. |
| A loop inside insist ends | Reconciliation, the tick and the acknowledgement stream each run forever. If one returns or panics, insist logs which one and exits with an error, and the service manager restarts it — it never goes on answering health checks and scrapes with a dead loop behind them. |
| insist hangs | Its unit is `Type=notify` with a `WatchdogSec` timeout; a ping sent from the tick loop must arrive before that timeout, or the service manager kills and restarts it. |
| The alerting system itself is down | The watchdog ping stops (dead man's switch again). Known instances keep escalating — they are never treated as resolved just because reconciliation cannot currently ask. |
| The dead man's switch refuses the ping or cannot be reached | `/watchdog` answers 502, so the alerting system retries it. Every failed forward counts in `insist_watchdog_forward_failures_total`, and `insist_watchdog_last_success_timestamp_seconds` stops advancing; the switch's URL appears in no log line. If the pings stay away, the switch raises its own alarm from outside. |
| The alerting API answers with an error | An error from that API is never read as "nothing is firing". Only an HTTP 200 with a valid JSON array of alerts counts as an answer; anything else leaves the last known state untouched, and the dead man's switch ping ages. |
| The push service is down | Mail keeps working. A step counts as sent only once the push service actually accepts it; otherwise it stays due and is retried on the next tick. If an instance is still unacknowledged after about an hour, its "unacknowledged" alert still reaches mail on its own route regardless. |
| The acknowledgement stream breaks | insist reconnects with backoff, resuming from its last saved cursor. A press that cannot be read yet simply has not been read yet: the instance keeps escalating exactly as if nobody had pressed anything. Every line the push service sends on the stream, its periodic keepalives included, moves `insist_ack_stream_last_event_timestamp_seconds`; a stream that is gone without an error shows as that gauge no longer advancing. |
| The state file is unreadable | It is moved aside with a timestamp in its name, a counter records that this happened, and insist starts again with an empty state. Every alert that is actually still firing announces itself again on the next reconciliation. |
| insist restarts | Its state lives on disk, so a restart is not a fresh start: the first reconciliation after coming back up re-establishes every instance that is still firing, and the acknowledgement stream resumes from its saved cursor rather than replaying or skipping. |
| A webhook body cannot be parsed | Unknown fields are ignored; a body that cannot be read at all answers with a server error, so the alerting system's own retry delivers it again. |
| A silence ends while the alert underneath is still firing | The same instance simply continues escalating according to its own age — a silence ending is not a new event to it. |
| An alert's `startsAt` lies in the future | Escalation is not switched off: every age is measured from the earlier of `startsAt` and the moment insist first saw the instance (see below). The first sighting more than a minute ahead logs one line and counts in `insist_future_starts_total`. |

A single pass of due sends is itself bounded: once the push service says
something about **itself** — as opposed to refusing one message — the
remaining sends in that pass are left for the next one, rather than each
waiting out its own multi-second timeout in turn. Without that bound, a
push service that merely hangs could hold the pass open long enough for
insist's own watchdog to kill it — turning a service that is only waiting
on a slow network call into one that gets restarted for it.

Since 0.2.4 that bound covers a refusal that is about the service as well:
**429 and 5xx**, not only an unreachable one. The reason is a measurement,
not a tidiness argument. Every webhook ends with a tick, a tick offers every
open instance that is due, and a failed send leaves `last_sent` alone — so
before 0.2.4 each webhook in a storm retried every alert that had arrived
before it. Against a fake push service answering 429, around 500 alerts
became **128100 requests in 100 seconds**: the amplification arrived exactly
when the service was already saying it had had enough. With the bound, a
refusing service costs one request per tick, whatever the number of open
instances.

A refusal that is about **one message** (a 400, a 413, a 401) deliberately
does not halt the pass. One malformed notification must not hold up every
other alert in the house — that would be the amplification's mirror image,
a single bad message silencing everything.

The deliberate non-feature here is a per-instance backoff. With the bound in
place, a refusing service already costs one request per tick; a backoff would
only add delay to the first delivery after it recovers. This program must
never make the alert path weaker, and an alert arriving late because insist
decided to wait is exactly that. Every send in
a pass that did not succeed — whether left over because the pass was
already halted, or attempted and refused by the push service itself — is
counted in the `insist_publish_pending` metric; an external alert watching
that metric for a value above zero for fifteen minutes is how "insist is
not getting messages out" becomes visible on its own, even while the
actual pushes are not going out.

An alert whose `startsAt` lies in the future must not switch escalation
off. It happens: Alertmanager 0.31.1 fills an omitted `startsAt` with the
posted `endsAt` (measured 2026-09-14), and a producer's clock can run ahead.
Measured from such a `startsAt`, an instance's age would stay at zero until
that moment — the first notice goes out, and then nothing, possibly for a
day. Every age insist derives (the ladder's steps, the minutes in the
"unacknowledged" alert, the time a notification says it started) is
therefore measured from the earlier of `startsAt` and the moment insist
first saw the instance; for a correct producer nothing changes. The fault
itself stays visible: the first time insist records an instance more than
a minute ahead of its own clock, it logs one line and counts it in
`insist_future_starts_total`. insist's own "unacknowledged" alert always
sends `startsAt` explicitly.

## Rejected

Using an existing "silence" mechanism as an acknowledgement was rejected:
a silence covers a label set for a fixed time window, not one specific
firing. Silencing something that later fires again for an unrelated reason
would silently suppress the new occurrence too — exactly the outcome this
project exists to prevent.

Keeping insist's own alert store, populated only from webhooks and never
reconciled against the alerting API, was rejected as well: a webhook can be
lost, and a store that only ever grows on its own signal has no way to
learn that something was actually resolved. Reconciliation against the API
is what lets insist recover the true state after its own restart, and what
lets a lost "resolved" webhook still get caught.
