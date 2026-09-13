{ pkgs, module, package }:
let
  webhook = ../fixtures/alertmanager-0.31.1/webhook-firing-single.json;
in
pkgs.testers.runNixOSTest {
  name = "insist";

  nodes.machine = { ... }: {
    imports = [ module ];
    environment.systemPackages = [ pkgs.curl pkgs.jq ];

    # ntfy configured like the homeserver: deny-all, a publishing user with a
    # token from `ntfy token generate`, not a hand-made string.
    systemd.services.ntfy-prepare = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
      path = [ pkgs.ntfy-sh pkgs.mkpasswd pkgs.coreutils ];
      script = ''
        mkdir -p /run/ntfy-test /var/lib/ntfy-test
        token=$(ntfy token generate)
        printf '%s' "$token" > /run/ntfy-test/alarm-token
        hash=$(mkpasswd -m bcrypt -R 10 "$(head -c 18 /dev/urandom | base64)")
        cat > /run/ntfy-test/server.yml <<EOF
        base-url: "http://127.0.0.1:2586"
        listen-http: "127.0.0.1:2586"
        auth-file: "/var/lib/ntfy-test/user.db"
        auth-default-access: "deny-all"
        cache-file: "/var/lib/ntfy-test/cache.db"
        auth-users:
          - "alarm:$hash:user"
        auth-tokens:
          - "alarm:$token"
        auth-access:
          - "alarm:alarmtopic*:rw"
        EOF
        printf 'NTFY_TOPIC=alarmtopic\nNTFY_TOKEN=%s\n' "$token" > /run/ntfy-test/insist-env
        chmod 0400 /run/ntfy-test/insist-env
      '';
    };
    systemd.services.ntfy-test = {
      wantedBy = [ "multi-user.target" ];
      after = [ "ntfy-prepare.service" ];
      requires = [ "ntfy-prepare.service" ];
      serviceConfig.ExecStart = "${pkgs.ntfy-sh}/bin/ntfy serve -c /run/ntfy-test/server.yml";
    };

    services.insist = {
      enable = true;
      inherit package;
      secretsFile = "/run/ntfy-test/insist-env";
      settings = {
        listen = "127.0.0.1:9099";
        ntfy_url = "http://127.0.0.1:2586";
        timezone = "Europe/Berlin";
        probe = { label = "insist_probe"; value = "ja"; topic_suffix = "-selbstprobe"; };
      };
    };
    systemd.services.insist = {
      after = [ "ntfy-prepare.service" "ntfy-test.service" ];
      requires = [ "ntfy-prepare.service" ];
    };
  };

  testScript = ''
    machine.wait_for_unit("ntfy-test.service")
    machine.wait_for_open_port(2586)
    machine.wait_for_unit("insist.service")
    machine.wait_for_open_port(9099)

    # Compare the code itself, never through grep -q under pipefail.
    code = machine.succeed(
        "curl -s -o /dev/null -w '%{http_code}' -X POST "
        "-H 'Content-Type: application/json' --data @${webhook} http://127.0.0.1:9099/"
    ).strip()
    assert code == "200", f"webhook answered {code}"

    # Measured at the result: the notification is in the topic.
    out = machine.succeed(
        "curl -s -H \"Authorization: Bearer $(cat /run/ntfy-test/alarm-token)\" "
        "'http://127.0.0.1:2586/alarmtopic/json?poll=1'"
    )
    assert "UnitFehlgeschlagen" in out, out
    assert "lan6-set.service" in out, out
  '';
}
