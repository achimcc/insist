# insist — design

## Why

On 2026-09-11 a firewall rule silently fell away and stayed off for five
hours. A watcher noticed and mailed about it every fifteen minutes from
17:49 to 22:27 — 21 mails, every one delivered, none read — plus one urgent
push notification at 17:55 that also went unread. The outage was found by a
human stumbling into it, not by any of the alerts.

The alerting was not missing coverage: both paths had seen the fault and
said so. What was missing was state. An alert has no memory of whether
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

A webhook's group can carry several alerts at once (they share a
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
as a gap in reconciliation, not as good news.

## Ladders and night

Each severity has its own ladder: a table of steps, each with a delay since
the alert started, an optional repeat interval, and a priority to send at.
`critical` escalates until acknowledged; `warning` reminds a few times;
anything else without a matching ladder falls back to a single, stateless
notice with no button.

A `warning` ladder can be marked quiet at night: its first notice, if due
during the configured night hours, goes out at low priority instead of
being held back entirely — deferred, never skipped. When the night ends,
the step that the alert's age actually deserves by then is sent at full,
loud priority for that step. An alert is never silently swallowed by the
clock.

A `critical` ladder can name one step as the point where an instance is
raised as its own alert back into the alerting system if it is still
unacknowledged — the equivalent of the earlier mail that a mounting pile of
identical reminders eventually gets ignored, sent instead as one alert with
its own, separate, mail-only route. That raised alert is marked with the
label `insist="unacknowledged"`; insist itself ignores any alert carrying
that label, so it can never end up watching, escalating or raising an
alert about itself.

## Acknowledging

Every notification for a `critical`, `warning` or `probe` instance carries
an "Acknowledge" button. Pressing it posts a short signed body to a topic
of its own — `a1.<instance-id>.<mac>` — where `<mac>` authenticates the
instance id against a key only insist holds. The button's own ntfy token
can only write to that one topic; it cannot read alerts, and it cannot post
anything to the alert topic itself. A button token that leaks therefore
cannot forge an alert with a working button, and can only ever acknowledge
the one specific instance named in a body it captured — nothing more.

## Failure behaviour

Every failure inside insist is designed to be either louder or visible
through some other, independent path — never silent. Mail is never routed
through insist: it goes directly from the alerting system, so a dead or
wedged insist never removes the household's oldest and most boring alerting
channel. The external dead man's switch is pinged only after a
reconciliation pass has actually succeeded and is still fresh; a hung or
crashed insist starves it, and the switch itself raises the alarm from
outside.

A single pass of due sends is bounded: once the push service stops
answering rather than merely refusing a message, the remaining sends of
that pass are left for the next one instead of being retried one by one at
a multi-second timeout each. Without that bound, a hung push service could
hold the pass open long enough for the process's own systemd watchdog to
kill it — turning a service that is merely waiting on a slow network call
into one that gets restarted for it. The count of sends left over from a
bounded pass is exposed as the `insist_publish_pending` metric, which is
also what an external alert on "insist is not sending" watches: an
escalation that is due but stuck behind a hung push service is a fact insist
can report about itself even while the actual pushes are not going out.

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
