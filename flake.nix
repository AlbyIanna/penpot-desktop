{
  # Penpot Local — Nix flake (M4 deliverable: dev shell + best-effort package).
  #
  # AUTHORED WITHOUT A LOCAL `nix` BINARY (macOS dev machine has no Nix).
  # Validation happens in CI. See nix/README.md for exactly what is covered,
  # what is aspirational, and how the NixOS-VM exit-criterion test would run.
  #
  # Scope (honest):
  #   * devShells.default — everything needed to hack on the repo and run the
  #     regression suites in dev mode (rust stable, JDK 26, valkey, ImageMagick,
  #     python3, just, node, and the Linux webkit2gtk/gtk stack for Tauri v2).
  #   * packages.headless — the headless supervisor/sync binary plus the
  #     penpot-watchdog deadman bin, wrapped so java/valkey/identify/node
  #     resolve from the Nix store. This is NOT the full GUI AppImage; the
  #     Tauri GUI bundle under pure Nix is out of M4 scope (documented gap).
  description = "Penpot Local — local-first desktop app wrapping Penpot's open-source stack";

  inputs = {
    # nixpkgs-unstable is REQUIRED, not a convenience: the pinned Penpot
    # backend (2.16.2 / upstream develop) is compiled for JDK 26 and launched
    # with --enable-preview, which hard-fails on any other JDK major. Release
    # channels (25.05 etc.) lag on new JDK majors; unstable carries jdk26.
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachSystem
      [
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ]
      (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          inherit (pkgs) lib stdenv;

          # ── JDK pin ────────────────────────────────────────────────────────
          # The backend jar runs with --enable-preview, which hard-fails unless
          # the JVM major is EXACTLY the one the jar was built with (26 for the
          # pinned Penpot 2.16.2 / upstream develop) — see PLAN.md "Stack".
          # nixpkgs-unstable (locked 2026-07-12) has no source-built `jdk26`
          # yet (jdk25 is the max); `temurin-bin-26` is the Adoptium binary
          # distribution of JDK 26.0.1 and covers all three target systems
          # (verified by evaluation: version/home/platforms all check out).
          # Deliberately no `or pkgs.jdk` fallback: a silent different major
          # would evaluate fine and then break at runtime. If nixpkgs later
          # grows `jdk26`, switching to it is fine — same major, either works.
          jdk = pkgs.temurin-bin-26;

          # ── Linux Tauri v2 link/runtime deps ───────────────────────────────
          # tauri → wry → webkit2gtk-4.1 + libsoup3 + the gtk3 stack, resolved
          # via pkg-config at compile time. Needed even for the headless bin:
          # the `headless` binary links the penpot-desktop lib, whose package
          # depends on the `tauri` crate.
          linuxTauriLibs = with pkgs; [
            gtk3
            webkitgtk_4_1
            libsoup_3
            glib
            glib-networking
            cairo
            pango
            gdk-pixbuf
            atk
            librsvg
            libayatana-appindicator
            openssl
            dbus
          ];

          # Tools every developer needs regardless of platform. Mirrors the
          # dev-mode dependency list in PLAN.md / docs/milestones/m1.md:
          # java (JDK 26), valkey-server, ImageMagick `identify` (backend
          # hard-fails on media upload without it), node (backend shells to
          # scripts/svgo-cli.js for SVG media), python3 (roundtrip/m2/m3
          # helpers), just (task runner).
          commonTools = with pkgs; [
            # Rust stable from nixpkgs (the repo has no rust-toolchain file;
            # workspace is edition 2021, any recent stable works).
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            cargo-tauri # `just dev` runs `cargo tauri dev`
            jdk
            valkey
            imagemagick
            nodejs
            python3
            just
            pkg-config
          ];

          # ── packages.headless ──────────────────────────────────────────────
          # Builds two workspace binaries:
          #   * apps/desktop `headless`      — full stack, no window (the bin
          #     the m1/m2/m3 regression scripts drive)
          #   * crates/supervisor `penpot-watchdog` — the SIGKILL deadman;
          #     MUST ship next to the app (docs/milestones/m3.md), so we build
          #     it in the same derivation and point PENPOT_WATCHDOG_BIN at it.
          headless = pkgs.rustPlatform.buildRustPackage {
            pname = "penpot-local-headless";
            version = "0.1.0"; # keep in sync with [workspace.package] in Cargo.toml

            # `self` is the git-tracked tree only, so gitignored runtime/ and
            # target/ never enter the store.
            src = self;
            cargoLock.lockFile = ./Cargo.lock;

            # Multiple -p/--bin pairs: cargo unions package selection and
            # builds each named bin from whichever selected package owns it.
            cargoBuildFlags = [
              "--package"
              "penpot-desktop"
              "--bin"
              "headless"
              "--package"
              "supervisor"
              "--bin"
              "penpot-watchdog"
            ];

            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.makeWrapper
            ];
            buildInputs =
              lib.optionals stdenv.isLinux linuxTauriLibs
              # Modern nixpkgs darwin stdenv ships the full Apple SDK
              # (frameworks included); libiconv is the one common extra Rust
              # link dep.
              ++ lib.optionals stdenv.isDarwin [ pkgs.libiconv ];

            # Workspace tests spawn embedded Postgres (network download on
            # fresh dirs) and bind localhost ports — impossible/flaky inside
            # the sandbox. CI runs `nix develop -c cargo test --workspace`
            # instead, outside the build sandbox.
            doCheck = false;

            # Wrap so the runtime tool lookups resolve from the Nix store:
            #   PENPOT_LOCAL_JAVA / PENPOT_LOCAL_VALKEY — AppConfig::resolve
            #     defaults (apps/desktop/src/lib.rs) are "java"/"valkey-server"
            #     on PATH; --set-default keeps user overrides working.
            #   PENPOT_WATCHDOG_BIN — supervisor resolves env → sibling; the
            #     sibling lookup would also work ($out/bin) but be explicit.
            #   PATH prefix — backend execs `identify` (ImageMagick) and
            #     `node` (scripts/svgo-cli.js) by name.
            # NOT provided: the Penpot runtime itself (penpot.jar + frontend).
            # That is fetched from the pinned docker images by
            # scripts/fetch-penpot.sh and pointed at via
            # PENPOT_LOCAL_RUNTIME_DIR. See nix/README.md.
            postInstall = ''
              wrapProgram $out/bin/headless \
                --set-default PENPOT_LOCAL_JAVA ${jdk}/bin/java \
                --set-default PENPOT_LOCAL_VALKEY ${pkgs.valkey}/bin/valkey-server \
                --set-default PENPOT_WATCHDOG_BIN $out/bin/penpot-watchdog \
                --prefix PATH : ${
                  lib.makeBinPath [
                    pkgs.imagemagick
                    pkgs.nodejs
                  ]
                }
            '';

            meta = {
              description = "Penpot Local headless stack (supervisor + proxy + sync daemon, no window)";
              homepage = "https://github.com/AlbyIanna/penpot-desktop";
              license = lib.licenses.mpl20;
              mainProgram = "headless";
              platforms = [
                "aarch64-darwin"
                "x86_64-linux"
                "aarch64-linux"
              ];
            };
          };
        in
        {
          packages = {
            inherit headless;
            default = headless;
          };

          apps.default = {
            type = "app";
            program = "${headless}/bin/headless";
            meta = headless.meta;
          };

          devShells.default = pkgs.mkShell {
            packages = commonTools;
            # pkg-config discovery for the Tauri build on Linux.
            buildInputs = lib.optionals stdenv.isLinux linuxTauriLibs;

            shellHook =
              ''
                export JAVA_HOME=${jdk.home}
                # Dev-mode defaults match the homebrew workflow in CLAUDE.md;
                # only set when absent so an explicit override always wins.
                export PENPOT_LOCAL_JAVA="''${PENPOT_LOCAL_JAVA:-${jdk}/bin/java}"
                export PENPOT_LOCAL_VALKEY="''${PENPOT_LOCAL_VALKEY:-${pkgs.valkey}/bin/valkey-server}"
              ''
              + lib.optionalString stdenv.isLinux ''
                # GTK/WebKit apps launched from a nix shell need schemas and
                # TLS gio modules on their search paths.
                export XDG_DATA_DIRS=${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:''${XDG_DATA_DIRS:-}
                export GIO_MODULE_DIR=${pkgs.glib-networking}/lib/gio/modules
              '';
          };

          # `nix flake check` builds this (slow but it IS the validation this
          # flake needs, given it was authored without a local nix).
          checks = {
            inherit headless;
          };
        }
      );
}
