# insist

Keeps alerts open and escalating until a human acknowledges them.

insist sits behind Prometheus Alertmanager, sends each alert to ntfy with an
"Acknowledge" button, and repeats it louder until the button is pressed or
Alertmanager resolves the alert. Mail is never routed through it.

Licence: AGPL-3.0-only.
