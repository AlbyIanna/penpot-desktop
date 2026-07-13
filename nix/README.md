# Nix flake — status, scope, and how to validate it

The M4 "Nix flake (dev shell + package)" deliverable. **The dev machine has no
`nix` binary** (macOS, homebrew workflow), but the flake was validated through
a `nixos/nix` docker container (docker daemon was available):

| Validation step | Status |
|---|---|
| `nix flake lock` (generated the committed `flake.lock`, nixpkgs-unstable @ 2026-07-12) | ✅ ran green |
| `nix flake check --no-build --all-systems` (full evaluation of every output on all 3 systems) | ✅ "all checks passed!", zero warnings |
| `nix build .#headless.cargoDeps` (fetch + vendor all 352 crates from `Cargo.lock`) | ✅ built |
| `nix build .#headless` (actual compilation) | ❌ NOT run (too heavy for the authoring session) — **CI must do this** |
| `nix develop` (shell usability, `cargo check` inside it) | ❌ NOT run — **CI must do this** |
| Runtime (booting the stack from the nix-built binary) | ❌ NOT run |

So: syntax, evaluation, attribute names (including the JDK pin), platform
coverage, and crate vendoring are proven; compilation and runtime are not.

## What the flake provides

| Output | What it is |
|---|---|
| `devShells.default` | rust stable (nixpkgs rustc/cargo/clippy/rustfmt/rust-analyzer + cargo-tauri), **JDK 26** (`temurin-bin-26`, see below), valkey, ImageMagick, node, python3, just, pkg-config, and on Linux the full Tauri v2 webkit2gtk-4.1/gtk3 stack plus `XDG_DATA_DIRS`/`GIO_MODULE_DIR` hooks. Exports `JAVA_HOME`; defaults `PENPOT_LOCAL_JAVA`/`PENPOT_LOCAL_VALKEY` to store paths only when unset (explicit overrides always win). |
| `packages.headless` (also `packages.default`, `apps.default`, `checks.headless`) | `rustPlatform.buildRustPackage` over the workspace `Cargo.lock`, building two bins: `headless` (full stack, no window — the bin the m1/m2/m3 scripts drive) and `penpot-watchdog` (the SIGKILL deadman that must ship next to the app). `headless` is wrapped: `PENPOT_LOCAL_JAVA`, `PENPOT_LOCAL_VALKEY`, `PENPOT_WATCHDOG_BIN` point into the store; `identify`/`node` are prefixed onto `PATH`. |

Systems: `aarch64-darwin`, `x86_64-linux`, `aarch64-linux`.

### The JDK pin: `temurin-bin-26`, not `jdk26`

The backend jar runs with `--enable-preview`, which hard-fails unless the JVM
major is **exactly** the one the jar was built with (26 for Penpot 2.16.2 /
upstream develop). nixpkgs-unstable as of the locked revision has no
source-built `jdk26` (jdk25 is the max — verified by evaluation, which is how
the original `pkgs.jdk26` pin was caught and fixed). `temurin-bin-26` is the
Adoptium binary distribution of JDK **26.0.1** — the same version as the dev
machine's homebrew JDK — with `.home` set and all three target systems in
`meta.platforms` (all verified by evaluation). If nixpkgs later grows `jdk26`,
switching is fine; never substitute a different major.

## What the flake does NOT cover (honest gaps)

1. **No GUI AppImage/dmg under pure Nix.** The nix package is the *headless*
   app only. The full Tauri GUI bundle is produced by the non-nix packaging
   pipeline; a `nix build` of the GUI is aspirational (it would need
   `cargo-tauri`'s bundler inside the sandbox plus icon/dist wiring).
2. **No Penpot runtime in the store.** `penpot.jar` + the static frontend are
   extracted from the pinned `penpotapp/{backend,frontend}:2.16.2` docker
   images by `scripts/fetch-penpot.sh` (docker required) and pointed at via
   `PENPOT_LOCAL_RUNTIME_DIR`. A pure-nix fetch of those images
   (`dockerTools.pullImage` + layer extraction) is the obvious future step;
   not done in M4.
3. **First boot is not offline, and embedded Postgres is a NixOS hazard.**
   `postgresql_embedded` downloads portable (theseus-rs) Postgres 15.18
   binaries from GitHub into the data dir on first run. Two consequences:
   - not offline-first under nix (the packaged-app bundle solves this by
     pre-seeding `penpot-runtime/postgres/`; the nix package does not);
   - those portable binaries assume an FHS dynamic linker and **will not run
     on stock NixOS** without `programs.nix-ld.enable = true;` (or an FHS
     wrapper like `steam-run`). The proper fix — teaching the supervisor to
     accept a pre-provisioned Postgres installation dir and pointing it at
     `pkgs.postgresql_15` — becomes easy once the M4 bundle resolution layer
     (`penpot-runtime/postgres/`) lands, and should be wired into this flake
     then.
4. **`doCheck = false`.** Workspace tests spawn embedded Postgres and bind
   localhost ports — sandbox-hostile. CI compensates with
   `nix develop -c cargo test --workspace` outside the build sandbox.
5. **Compilation is unproven** (see the validation table): the first
   `nix build .#headless` may still surface a missing Linux link dep (the
   pkg-config error will name the `.pc` file — add the lib to
   `linuxTauriLibs`) or a darwin framework issue (modern nixpkgs darwin
   stdenv bundles the full Apple SDK, so this is unlikely).

## Dev-mode compatibility

The flake is additive. Nothing in the repo reads flake outputs; the homebrew
dev path (`/opt/homebrew/opt/openjdk`, repo `runtime/`, `just dev|smoke|invariant|m3`)
is untouched. The dev shell only *defaults* `PENPOT_LOCAL_JAVA`/`PENPOT_LOCAL_VALKEY`
when they are unset.

## CI validation (what the CI job must run)

On `x86_64-linux` with nix ≥ 2.19 (flakes enabled), repo root. `flake.lock`
is already committed — do NOT re-lock unless deliberately bumping inputs.

```bash
# 1. evaluation of every output, all systems (fast; already proven green once)
nix flake check --no-build --all-systems

# 2. THE step the authoring session could not run: compile the package
nix build .#headless -L
test -x result/bin/headless && test -x result/bin/penpot-watchdog

# 3. the dev shell actually compiles the workspace
nix develop -c cargo check --workspace

# 4. full check (builds checks.headless — mostly cached after step 2)
nix flake check -L

# 5. (optional, needs network + free ports — must NOT run inside a nix build)
nix develop -c cargo test --workspace
```

Expected failure modes for step 2, in order of likelihood: a missing Linux
link dep in `linuxTauriLibs` (pkg-config error names the .pc file), a crate
build script wanting a tool not in `nativeBuildInputs`.

## The NixOS-VM exit-criterion test (when someone has nix)

M4 exit criterion: *a fresh machine (or clean NixOS VM) runs the app from a
single artifact.* For the nix flavor of that test, on a clean NixOS VM with
docker enabled (`virtualisation.docker.enable = true;`) and
`programs.nix-ld.enable = true;` (see gap 3):

```bash
# 1. build the app from the flake, nothing preinstalled
nix build github:AlbyIanna/penpot-desktop#headless   # private repo: use a token or a local checkout

# 2. materialize the pinned Penpot runtime (docker create/cp/rm only, no containers run)
git clone https://github.com/AlbyIanna/penpot-desktop && cd penpot-desktop
nix develop -c bash scripts/fetch-penpot.sh --no-java-check

# 3. boot the stack headless
PENPOT_LOCAL_RUNTIME_DIR=$PWD/runtime \
PENPOT_LOCAL_DATA_DIR=$HOME/.local/share/penpot-local \
  ./result/bin/headless
# expect "READY http://127.0.0.1:8686" on stdout, then:
curl -fsS http://127.0.0.1:8686 | grep -qi penpot

# 4. the invariant, on nix-provided tooling
nix develop -c bash scripts/m2-invariant.sh
```

Until the supervisor can consume a nixpkgs Postgres (gap 3), step 3 on NixOS
additionally needs nix-ld; on non-NixOS Linux and macOS it should work as-is.

## How this flake was validated without local nix (repeatable)

```bash
# stage a .git-less copy (so untracked files are visible to the flake) and run nix in docker:
rsync -a --exclude .git --exclude target --exclude runtime ./ /tmp/flakecheck/
docker run --rm -v /tmp/flakecheck:/src -w /src nixos/nix \
  nix --extra-experimental-features 'nix-command flakes' flake check --no-build --all-systems
```
