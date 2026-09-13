{ config, lib, pkgs, ... }:
let
  cfg = config.services.insist;
  settings = cfg.settings // {
    secrets_file = "/run/credentials/insist.service/insist-env";
  };
  configFile = (pkgs.formats.toml { }).generate "insist.toml" settings;
in
{
  options.services.insist = {
    enable = lib.mkEnableOption "insist, alerts that stay open until acknowledged";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The insist package to run.";
    };

    settings = lib.mkOption {
      type = (pkgs.formats.toml { }).type;
      description = ''
        Contents of insist.toml, except `secrets_file`, which this module
        sets. NO SECRET BELONGS HERE: this becomes a world-readable store file.
      '';
    };

    secretsFile = lib.mkOption {
      type = lib.types.str;
      description = ''
        Path of the KEY=VALUE credential file OUTSIDE the store, handed to
        the unit with LoadCredential. A string, not a path: a Nix path would
        copy the secret into the store.
      '';
      example = "/run/host/credentials/insist-env";
    };

    extraServiceConfig = lib.mkOption {
      type = lib.types.attrsOf lib.types.anything;
      default = { };
      description = "Extra serviceConfig attributes, merged over this module's own.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.insist = {
      description = "insist: alerts that stay open until acknowledged";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      # [Unit] keys: under serviceConfig systemd ignores the interval and the
      # limit never trips (learned in signal-seerr's module).
      unitConfig = {
        StartLimitBurst = 5;
        StartLimitIntervalSec = 300;
      };
      serviceConfig = {
        # notify: insist reports READY after its first reconciliation, and
        # pings the watchdog from its tick loop. A hung loop is restarted by
        # systemd; a dead one starves the dead man's switch.
        Type = "notify";
        NotifyAccess = "main";
        WatchdogSec = "120s";
        ExecStart = "${lib.getExe cfg.package} ${configFile}";
        Restart = "on-failure";
        RestartSec = "10s";
        LoadCredential = "insist-env:${cfg.secretsFile}";
        DynamicUser = true;
        StateDirectory = "insist";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectKernelTunables = true;
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
        SystemCallFilter = [ "@system-service" ];
      } // cfg.extraServiceConfig;
    };
  };
}
