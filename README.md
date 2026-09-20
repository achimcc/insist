# insist

Keeps alerts open and escalating until a human acknowledges them.

insist sits behind Prometheus Alertmanager, sends each alert to ntfy with an
"Acknowledge" button, and repeats it louder until the button is pressed or
Alertmanager resolves the alert. Mail is never routed through it.

See [`docs/design.md`](docs/design.md) for the design.

Licence: AGPL-3.0-only.

## Configuration

insist reads one TOML file, given on the command line, plus the credential
file it names (see "Credential file" below — no secret belongs in the TOML
file itself). Below is the configuration this repository's own tests load
as their minimal, valid config (`fixtures/constructed-config.toml`), with
every line explained:

```toml
listen = "127.0.0.1:9099"                        # host:port insist's own HTTP server binds to (webhook, /watchdog, /metrics, /health)
ntfy_url = "https://ntfy.example"                 # base URL of the ntfy server notifications are published to
alertmanager_url = "http://127.0.0.1:9093"        # base URL of the Alertmanager whose API insist reconciles against
secrets_file = "/run/credentials/insist.service/insist-env"  # KEY=VALUE credential file handed in by systemd, see "Credential file" below
state_file = "/var/lib/insist/state.json"         # where insist keeps step, last send and acknowledgement per instance
timezone = "Europe/Berlin"                        # timezone the night window and displayed clock times are computed in
ack_topic_suffix = "-quittung"                    # suffix appended to the alarm topic name to get the acknowledgement topic
reconcile_secs = 60                               # how often insist asks Alertmanager's API what is actually still firing
tick_secs = 15                                    # how often insist checks every open instance against its ladder
watchdog_max_age_secs = 300                       # a reconciliation older than this makes /watchdog answer 503 instead of forwarding the ping
unacknowledged_alertname = "AlarmUnquittiert"     # alertname insist raises back into Alertmanager once a critical step's raise_unacknowledged fires
# "rec" is the receiver name used when recording fixtures/alertmanager-0.31.1.
receivers = ["rec"]                               # Alertmanager receivers whose webhook points at insist; every other receiver is ignored on reconciliation

[probe]                                           # settings for the self-test probe alert
label = "insist_probe"                            # label name that marks an alert as a self-test probe
value = "ja"                                      # label value that marks an alert as a self-test probe
topic_suffix = "-selbstprobe"                     # suffix appended to the alarm topic name for probe notifications, so a probe never reaches a phone

[night]                                           # the quiet window a warning ladder can defer its first loud notice around
start_hour = 22                                   # hour (local time, see "timezone" above) the quiet window starts
end_hour = 7                                      # hour (local time) the quiet window ends

[ladders.critical]                                # the escalation table for severity "critical"
steps = [                                         # each entry: how long after the alert started, how often to repeat, how loud
  { after_secs = 0, priority = 4 },                                                    # first notice, as soon as the instance is seen
  { after_secs = 900, repeat_secs = 900, priority = 5 },                               # from 15 minutes on: repeat every 15 minutes, loud
  { after_secs = 3600, repeat_secs = 300, priority = 5, raise_unacknowledged = true },  # from 1 hour on: repeat every 5 minutes, and raise the "still unacknowledged" alert
]                                                  # end of the critical ladder's steps

[ladders.warning]                                 # the escalation table for severity "warning" (and for a missing severity)
quiet_at_night = true                             # a first notice due at night goes out quiet (priority 2) instead of being held back
steps = [                                         # each entry: how long after the alert started, how often to repeat, how loud
  { after_secs = 0, priority = 3 },                          # first notice
  { after_secs = 14400, repeat_secs = 43200, priority = 4 }, # one loud reminder after 4 hours, then every 12 hours
]                                                  # end of the warning ladder's steps

[ladders.probe]                                   # the escalation table used only for the self-test probe alert
steps = [                                         # each entry: how long after the alert started, how often to repeat, how loud
  { after_secs = 0, priority = 4 },                        # first notice
  { after_secs = 60, repeat_secs = 60, priority = 5 },     # loud after 60 s, repeating every 60 s
]                                                  # end of the probe ladder's steps
```

## Credential file

The file named by `secrets_file` holds `KEY=VALUE` lines and nothing else.
It must never live in the Nix store (a world-readable path) — it is handed
in outside of Nix, for instance via systemd's `LoadCredential`. Five keys
are required:

- `NTFY_TOPIC` — the alarm topic. Its name is itself a secret: anyone who
  knows it, and holds any token with read access, can read the alerts.
- `NTFY_TOKEN` — the publishing token insist uses to post to that topic.
- `NTFY_BUTTON_TOKEN` — the token embedded in every "Acknowledge" button.
  It must be granted write access to the acknowledgement topic only, and
  nothing else — enforcing that is the installer's job, via ntfy's own
  access control, not insist's.
- `ACK_HMAC_KEY` — the key that signs and verifies acknowledgement bodies.
- `WATCHDOG_URL` — the URL of the external dead man's switch. Its URL is
  itself the credential that authorises pinging it.

## NixOS

The flake exports a module as `nixosModules.default`:

```nix
services.insist = {
  enable = true;
  package = insist.packages.${pkgs.system}.default;
  secretsFile = "/run/host/credentials/insist-env"; # outside the store; a string, not a path
  settings = {
    # see "Configuration" above — everything except secrets_file, which
    # this module sets for you
  };
};
```

## What it will not do

- **No mail.** Mail is sent directly by Alertmanager (or whatever raised
  the alert), never through insist — a dead or wedged insist must not take
  the plainest, most established alerting channel there is down with it.
- **No silences.** Acknowledging an alert in insist is bound to one firing
  instance, not to a label set over a time window; insist has no notion of
  silencing future, unrelated alerts.
- **No web UI.** The only way to acknowledge an alert is the button in the
  push notification. There is no page to click through, and none is
  planned.

## Deployed at

Running in the author's homeserver (an obs-01 guest) since 2026-09-14, v0.2.5.
