{
  description = "Android TV Voice over BLE (ATVV) daemon for Linux/PipeWire";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
  }:
    flake-utils.lib.eachSystem ["x86_64-linux" "aarch64-linux"] (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.mkLib pkgs;

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;

          nativeBuildInputs = with pkgs; [
            pkg-config
            rustPlatform.bindgenHook
          ];

          buildInputs = with pkgs; [
            pipewire
            dbus
          ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        atvvoice = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
          });
      in {
        checks = {
          inherit atvvoice;
        };

        packages.default = atvvoice;

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};

          packages = with pkgs; [
            rust-analyzer
          ];

          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
        };
      }
    )
    // {
      overlays.default = final: _prev: {
        atvvoice = self.packages.${final.stdenv.hostPlatform.system}.default;
      };

      homeManagerModules.default = self.homeManagerModules.atvvoice;
      homeManagerModules.atvvoice = {
        config,
        lib,
        pkgs,
        ...
      }: let
        cfg = config.services.atvvoice;
        inherit (lib) mkEnableOption mkOption mkIf types;
      in {
        options.services.atvvoice = {
          enable = mkEnableOption "atvvoice BLE voice remote daemon";

          package = mkOption {
            type = types.package;
            default = pkgs.atvvoice;
            defaultText = "pkgs.atvvoice";
            description = "The atvvoice package to use.";
          };

          device = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Bluetooth address of the remote (e.g. AA:BB:CC:DD:EE:FF). null = auto-detect first ATVV device.";
          };

          adapter = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "BlueZ adapter name. null = auto-detect.";
          };

          gain = mkOption {
            type = types.nullOr types.number;
            default = null;
            description = "Audio gain in dB. null = app default (20).";
          };

          mode = mkOption {
            type = types.nullOr (types.enum ["toggle" "hold"]);
            default = null;
            description = "Mic button mode. null = app default (toggle).";
          };

          frameTimeout = mkOption {
            type = types.nullOr types.int;
            default = null;
            description = "Seconds without audio frames before auto-closing mic. null = app default (5). 0 = disabled.";
          };

          idleTimeout = mkOption {
            type = types.nullOr types.int;
            default = null;
            description = "Seconds since last button press before auto-closing mic. null = app default (0/disabled).";
          };

          verbose = mkOption {
            type = types.nullOr (types.ints.between 0 3);
            default = null;
            description = "Log verbosity. null = app default (0/info).";
          };

          name = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Instance name suffix. Sets PipeWire node name (atvvoice-<name>) and D-Bus bus name (org.atvvoice.<name>).";
          };

          description = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "PipeWire node description (shown in audio settings). null = 'ATVVoice Microphone' or 'ATVVoice Microphone (<name>)'.";
          };

          noDbus = mkOption {
            type = types.bool;
            default = false;
            description = "Disable D-Bus control interface.";
          };
        };

        config = mkIf cfg.enable {
          systemd.user.services.atvvoice = {
            Unit = {
              Description = "ATVVoice - BLE voice remote to PipeWire virtual microphone";
              After = ["pipewire.service"];
              Requires = ["pipewire.service"];
            };
            Service = let
              args =
                lib.optionals (cfg.device != null) ["-d" cfg.device]
                ++ lib.optionals (cfg.adapter != null) ["-a" cfg.adapter]
                ++ lib.optionals (cfg.gain != null) ["-g" (toString cfg.gain)]
                ++ lib.optionals (cfg.mode != null) ["-m" cfg.mode]
                ++ lib.optionals (cfg.frameTimeout != null) ["--frame-timeout" (toString cfg.frameTimeout)]
                ++ lib.optionals (cfg.idleTimeout != null) ["--idle-timeout" (toString cfg.idleTimeout)]
                ++ lib.optionals (cfg.verbose != null) (lib.genList (_: "-v") cfg.verbose)
                ++ lib.optionals (cfg.name != null) ["--name" cfg.name]
                ++ lib.optionals (cfg.description != null) ["--description" cfg.description]
                ++ lib.optionals cfg.noDbus ["--no-dbus"];
            in {
              Type = "simple";
              ExecStart = "${cfg.package}/bin/atvvoice ${lib.escapeShellArgs args}";
              Restart = "on-failure";
              RestartSec = 5;
            };
            Install = {
              WantedBy = ["default.target"];
            };
          };
        };
      };
    };
}
