{ pkgs, module, package }:
pkgs.testers.runNixOSTest {
  name = "insist";

  nodes.machine = { ... }: {
    imports = [ module ];
    environment.systemPackages = [
      pkgs.curl
      pkgs.jq
      # One place for the token and the URL, so the test script below does
      # not nest shell quoting inside Python strings inside Nix.
      (pkgs.writeShellScriptBin "poll-topic" ''
        exec ${pkgs.curl}/bin/curl -s -H "Authorization: Bearer $(cat /run/ntfy-test/alarm-token)" "http://127.0.0.1:2586/$1/json?poll=1"
      '')
      (pkgs.writeShellScriptBin "press" ''
        exec ${pkgs.curl}/bin/curl -sf -X POST -H "Authorization: Bearer $(cat /run/ntfy-test/knopf-token)" --data "$1" http://127.0.0.1:2586/alarmtopic-quittung
      '')
    ];

    systemd.services.ntfy-prepare = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
      path = [ pkgs.ntfy-sh pkgs.mkpasswd pkgs.coreutils ];
      script = ''
        mkdir -p /run/ntfy-test /var/lib/ntfy-test
        alarm=$(ntfy token generate); knopf=$(ntfy token generate)
        printf '%s' "$alarm" > /run/ntfy-test/alarm-token
        printf '%s' "$knopf" > /run/ntfy-test/knopf-token
        h1=$(mkpasswd -m bcrypt -R 10 "$(head -c 18 /dev/urandom | base64)")
        h2=$(mkpasswd -m bcrypt -R 10 "$(head -c 18 /dev/urandom | base64)")
        cat > /run/ntfy-test/server.yml <<EOF
        base-url: "http://127.0.0.1:2586"
        listen-http: "127.0.0.1:2586"
        auth-file: "/var/lib/ntfy-test/user.db"
        auth-default-access: "deny-all"
        cache-file: "/var/lib/ntfy-test/cache.db"
        keepalive-interval: "5s"
        auth-users:
          - "alarm:$h1:user"
          - "knopf:$h2:user"
        auth-tokens:
          - "alarm:$alarm"
          - "knopf:$knopf"
        auth-access:
          - "alarm:alarmtopic*:rw"
          - "knopf:alarmtopic-quittung:write-only"
        EOF
        printf 'NTFY_TOPIC=alarmtopic\nNTFY_TOKEN=%s\nNTFY_BUTTON_TOKEN=%s\nACK_HMAC_KEY=%s\nWATCHDOG_URL=http://127.0.0.1:8099/ping\n' \
          "$alarm" "$knopf" "$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')" > /run/ntfy-test/insist-env
        chmod 0400 /run/ntfy-test/insist-env
      '';
    };
    systemd.services.ntfy-test = {
      wantedBy = [ "multi-user.target" ];
      after = [ "ntfy-prepare.service" ];
      requires = [ "ntfy-prepare.service" ];
      serviceConfig.ExecStart = "${pkgs.ntfy-sh}/bin/ntfy serve -c /run/ntfy-test/server.yml";
    };

    # A stand-in for the external dead man's switch: counts pings in a file.
    systemd.services.deadman-sink = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig.ExecStart = pkgs.writers.writePython3 "deadman-sink" { } ''
        import http.server


        class H(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", "0")))
                with open("/tmp/pings", "a") as f:
                    f.write("ping\n")
                self.send_response(200)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, *a):
                pass


        http.server.HTTPServer(("127.0.0.1", 8099), H).serve_forever()
      '';
    };

    services.prometheus.alertmanager = {
      enable = true;
      port = 9093;
      listenAddress = "127.0.0.1";
      configuration = {
        route = {
          receiver = "insist";
          # "probe_lauf" on top of "alertname": measured (2026-09-13), with
          # group_by on alertname alone, resolving the second probe alert
          # right after the restart below never produced a webhook — the
          # Alertmanager metric alertmanager_notification_requests_total for
          # the webhook integration stayed frozen at its prior count for
          # over 90 s, while the same resolve on a probe alone (its own
          # group) went out in single-digit seconds. Both probes share
          # alertname "InsistProbe"; grouping them together makes the
          # second one's notification ride on the first's already-recently-
          # flushed group timeline. Splitting the group by probe_lauf too
          # gives each probe alert its own aggregation group and its own
          # group_wait/group_interval clock.
          group_by = [ "alertname" "probe_lauf" ];
          group_wait = "1s";
          group_interval = "5s";
          repeat_interval = "1h";
          routes = [
            { matchers = [ ''alertname = "Watchdog"'' ]; receiver = "insist-watchdog"; group_wait = "0s"; group_interval = "5s"; repeat_interval = "10s"; }
          ];
        };
        receivers = [
          { name = "insist"; webhook_configs = [ { url = "http://127.0.0.1:9099/"; send_resolved = true; } ]; }
          { name = "insist-watchdog"; webhook_configs = [ { url = "http://127.0.0.1:9099/watchdog"; send_resolved = false; } ]; }
        ];
      };
    };

    services.insist = {
      enable = true;
      inherit package;
      secretsFile = "/run/ntfy-test/insist-env";
      settings = {
        listen = "127.0.0.1:9099";
        ntfy_url = "http://127.0.0.1:2586";
        alertmanager_url = "http://127.0.0.1:9093";
        state_file = "/var/lib/insist/state.json";
        timezone = "Europe/Berlin";
        ack_topic_suffix = "-quittung";
        reconcile_secs = 3;
        tick_secs = 2;
        watchdog_max_age_secs = 20;
        unacknowledged_alertname = "AlarmUnquittiert";
        # Only the webhook target's receiver: the watchdog's own receiver
        # must stay out, so a Watchdog alert is never treated as an instance
        # to notify and acknowledge.
        receivers = [ "insist" ];
        probe = { label = "insist_probe"; value = "ja"; topic_suffix = "-selbstprobe"; };
        night = { start_hour = 22; end_hour = 7; };
        ladders = {
          critical.steps = [
            { after_secs = 0; priority = 4; }
            { after_secs = 900; repeat_secs = 900; priority = 5; }
            { after_secs = 3600; repeat_secs = 300; priority = 5; raise_unacknowledged = true; }
          ];
          warning = {
            quiet_at_night = true;
            steps = [
              { after_secs = 0; priority = 3; }
              { after_secs = 14400; repeat_secs = 43200; priority = 4; }
            ];
          };
          probe.steps = [
            { after_secs = 0; priority = 4; }
            { after_secs = 20; repeat_secs = 20; priority = 5; }
          ];
        };
      };
    };
    systemd.services.insist = {
      after = [ "ntfy-test.service" "alertmanager.service" ];
      requires = [ "ntfy-prepare.service" ];
    };
  };

  testScript = ''
    import json

    def messages(topic):
        out = machine.succeed(f"poll-topic {topic}")
        return [m for m in (json.loads(l) for l in out.splitlines() if l.strip()) if m.get("event") == "message"]

    def wait_for(topic, jq_condition, timeout):
        # jq -e reads its whole input and sets the exit code; no grep -q under
        # pipefail, whose SIGPIPE turns a match into a failure.
        machine.wait_until_succeeds(f"poll-topic {topic} | jq -se '{jq_condition}' >/dev/null", timeout=timeout)

    def iso(offset):
        return machine.succeed(f"date -u -d '{offset}' +%Y-%m-%dT%H:%M:%SZ").strip()

    def post_alert(labels, starts, ends):
        # Alertmanager's own default for an OMITTED startsAt is not "now":
        # measured on this server (0.31.1), posting only endsAt made
        # `GET /api/v2/alerts` answer with startsAt == endsAt. With endsAt
        # ten minutes out, the ladder's age (now - startsAt) was pinned at 0
        # for the whole test and it never escalated. A real Prometheus rule
        # always sends both, so the fix is to always send both here too, and
        # to reuse the SAME startsAt for an alert's later resolving post
        # rather than defaulting a fresh one a second time.
        body = json.dumps([{"labels": labels, "annotations": {"summary": "probe"}, "startsAt": starts, "endsAt": ends}])
        machine.succeed(f"curl -sf -X POST -H 'Content-Type: application/json' --data '{body}' http://127.0.0.1:9093/api/v2/alerts")

    machine.wait_for_unit("ntfy-test.service")
    machine.wait_for_open_port(2586)
    machine.wait_for_unit("alertmanager.service")
    machine.wait_for_open_port(9093)
    machine.wait_for_unit("insist.service")
    machine.wait_for_open_port(9099)

    probe = {"alertname": "InsistProbe", "severity": "critical", "insist_probe": "ja"}

    with subtest("a probe alert arrives with a button, and only in the probe topic"):
        probe_starts = iso("now")
        post_alert(probe, probe_starts, iso("+10 minutes"))
        wait_for("alarmtopic-selbstprobe", 'any(.[]; .title == "InsistProbe")', 60)
        first = [m for m in messages("alarmtopic-selbstprobe") if m.get("title") == "InsistProbe"][0]
        assert first["priority"] == 4, first
        seq = first["sequence_id"]
        body = first["actions"][0]["body"]
        assert body.startswith("a1." + seq + "."), body
        assert messages("alarmtopic") == [], "a probe must not reach the alarm topic"

    with subtest("it escalates"):
        wait_for("alarmtopic-selbstprobe", 'any(.[]; .priority == 5)', 90)

    with subtest("a forged press is rejected and counted"):
        machine.succeed("press a1." + seq + ".00000000000000000000000000000000")
        machine.wait_until_succeeds("curl -s http://127.0.0.1:9099/metrics | jq -Rse 'test(\"insist_ack_rejected_total 1\")' >/dev/null", timeout=30)

    with subtest("the real press acknowledges and silences"):
        machine.succeed(f"press {body}")
        wait_for("alarmtopic-selbstprobe", 'any(.[]; (.title // "") | startswith("Acknowledged"))', 30)
        loud_before = len([m for m in messages("alarmtopic-selbstprobe") if m.get("priority") == 5])
        machine.sleep(45)
        loud_after = len([m for m in messages("alarmtopic-selbstprobe") if m.get("priority") == 5])
        assert loud_after == loud_before, f"escalated after acknowledgement: {loud_before} -> {loud_after}"

    with subtest("resolving sends Resolved and empties the state"):
        post_alert(probe, probe_starts, iso("-1 second"))
        wait_for("alarmtopic-selbstprobe", 'any(.[]; .title == "Resolved: InsistProbe")', 60)
        machine.wait_until_succeeds("jq -e '.instances | length == 0' /var/lib/insist/state.json >/dev/null", timeout=30)

    with subtest("a press made while insist is down is picked up after restart"):
        # A second, distinct label makes a new fingerprint (and thus a new
        # instance and sequence_id) so its messages can be told apart from
        # the first probe's, which is still sitting in the same topic.
        probe2 = dict(probe, probe_lauf="2")
        probe2_starts = iso("now")
        post_alert(probe2, probe2_starts, iso("+10 minutes"))
        wait_for(
            "alarmtopic-selbstprobe",
            f'any(.[]; .title == "InsistProbe" and .sequence_id != "{seq}")',
            60,
        )
        second = [
            m for m in messages("alarmtopic-selbstprobe")
            if m.get("title") == "InsistProbe" and m.get("sequence_id") != seq
        ][0]
        seq2 = second["sequence_id"]
        body2 = second["actions"][0]["body"]

        # The button itself talks straight to ntfy, not to insist: a press
        # can land while insist is down. What is under test is whether the
        # acknowledgement stream resumes from where it left off (its
        # recorded `since=<cursor>`) once insist comes back, instead of
        # replaying ntfy's whole cache (`since=all`) — which would also
        # re-deliver the still-wrong-tag forged press from above and reject
        # it a second time. `machine.systemctl` alone does not check its own
        # exit code, so the stop is asserted explicitly and confirmed with
        # `is-active` before the button is pressed while insist is down.
        machine.succeed("systemctl stop insist.service")
        machine.fail("systemctl is-active insist.service")
        machine.succeed(f"press {body2}")
        restart_epoch = machine.succeed("date +%s").strip()
        machine.succeed("systemctl start insist.service")
        machine.wait_for_unit("insist.service")

        wait_for(
            "alarmtopic-selbstprobe",
            f'any(.[]; .sequence_id == "{seq2}" and ((.title // "") | startswith("Acknowledged")))',
            60,
        )

        # since=all would have re-read the old forged press (its tag is
        # still wrong, so it would be rejected again) and bumped this
        # counter above zero; since=<cursor> never re-reads it. The counter
        # is process-local (it restarts at 0), so this is only meaningful
        # right after the restart above, before anything else can reject.
        machine.wait_until_succeeds(
            "curl -s http://127.0.0.1:9099/metrics | grep -qxF 'insist_ack_rejected_total 0'",
            timeout=10,
        )
        # A second replayed message since=all would misread: the real press
        # for the FIRST probe, already resolved and gone from state by now,
        # which insist logs as an unknown instance rather than a rejection
        # (so the counter above cannot see it). Check the journal instead,
        # scoped to after the restart.
        machine.fail(
            f"journalctl -u insist --since=@{restart_epoch} | grep -qi 'unknown instance'"
        )

        # Resolve it too, so the state is empty again before the watchdog
        # subtests below.
        post_alert(probe2, probe2_starts, iso("-1 second"))
        wait_for(
            "alarmtopic-selbstprobe",
            f'any(.[]; .sequence_id == "{seq2}" and .title == "Resolved: InsistProbe")',
            60,
        )

    with subtest("the watchdog ping passes through while reconciliation works"):
        post_alert({"alertname": "Watchdog", "severity": "none"}, iso("now"), iso("+10 minutes"))
        machine.wait_until_succeeds("test -s /tmp/pings", timeout=60)

    with subtest("without Alertmanager the ping is withheld"):
        machine.systemctl("stop alertmanager.service")
        machine.sleep(25)
        code = machine.succeed("curl -s -o /dev/null -w '%{http_code}' -X POST --data '{}' http://127.0.0.1:9099/watchdog").strip()
        assert code == "503", code

    with subtest("the systemd watchdog itself never had to restart insist"):
        # NRestarts counts only automatic restarts systemd's own Restart=
        # logic triggered (e.g. a missed WatchdogSec ping aborting the
        # process); a manual stop+start, like the one above, is not that
        # and does not increment it (systemd(1) NRestarts: "the number of
        # times the service has been restarted"; the restart subtest above
        # used an explicit stop then start, never `systemctl restart` or an
        # automatic Restart=). Read by NAME=, not by the order of the two
        # -p flags (systemctl show does not promise to keep that order).
        show = machine.succeed("systemctl show insist.service -p NRestarts -p WatchdogUSec").splitlines()
        assert "NRestarts=0" in show, show
        assert "WatchdogUSec=2min" in show, show
  '';
}
