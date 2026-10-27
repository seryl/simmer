# simmer

**Instant Nix dev environments.** simmer evaluates a project's Nix dev
shell once, caches the finished environment, and replays it into fish,
bash, or zsh at every prompt in a few milliseconds — no bash, no nix, no
subprocess on the hot path. When a watched file changes, the next prompt
rebuilds the environment *in the foreground*, with a live progress view,
and you get the fresh result before you type your next command.

It is a standalone replacement for the direnv + nix-direnv load path, and
it reads existing `.envrc` files, so a project needs no changes to use it.

```text
~/src/app $ cd .
⠼ warming ~/src/app  4.1s  1 running · 2 done · 0 failed · 0 queued
  ⠼ nix build  3.9s  building 'dev-shell.drv'
      │ unpacking sources
      │ configuring
      │ building
```

## Why

- **Fast where it matters.** The per-prompt check is a stat of the watched
  files and one string compare. Entering a project applies a cached script.
  Measured numbers are in [Performance](#performance).
- **Correct invalidation.** An environment is keyed by a content hash of the
  files that define it (`flake.nix`, `flake.lock`, `.envrc`, your extra
  `watch` list). Edit one and the next prompt rebuilds; touch nothing and
  nothing runs. It does not depend on Nix's eval cache, which a dirty git
  tree silently disables.
- **It waits, and tells you what it is doing.** A rebuild blocks the prompt
  with a live view (or plain lines in CI, or JSON for tools), propagates a
  failing step's exit status, keeps the failing step's log, and stops every
  process it started when you press Ctrl-C.
- **Safe by default.** Nothing in a project runs until you `simmer allow` it.
- **Works with what you have.** `.simmer.toml`, a bare `flake.nix`, or an
  existing `.envrc` all work. Editors that call `direnv` can be pointed at
  simmer through a `direnv`-named shim.

## Install

simmer needs [Nix](https://nixos.org/download) with flakes enabled
(`experimental-features = nix-command flakes`), `bash`, and Linux or macOS.

**With Nix:**

```sh
nix profile install github:seryl/simmer
```

or, in a flake-based NixOS or home-manager configuration, add
`github:seryl/simmer` as an input and put
`inputs.simmer.packages.${system}.default` in your packages.

**With Cargo** (Rust 1.82 or newer):

```sh
cargo install --locked --git https://github.com/seryl/simmer simmer
```

Either way you get two binaries: `simmer`, and `simmer-daemon` (the
optional pre-warm daemon, started with `simmer daemon`).

## Quickstart

1. **Hook your shell** (once):

   ```sh
   # fish — in ~/.config/fish/config.fish
   simmer hook fish | source
   # bash — in ~/.bashrc
   eval "$(simmer hook bash)"
   # zsh — in ~/.zshrc
   eval "$(simmer hook zsh)"
   ```

2. **Allow a project.** In a directory with a `flake.nix` (or `.envrc`, or
   `.simmer.toml`):

   ```sh
   cd ~/src/app
   simmer allow
   ```

   Entering a project you have not allowed prints a one-time hint saying so.

3. **That's it.** The next prompt builds the environment (with progress) and
   loads it. Leave the directory and it unloads; come back and it loads
   from the cache. Edit `flake.nix` and the next prompt rebuilds.

To try it without a flake of your own, [`examples/`](examples/) has a
small flake with a dev shell, a manifest, and an annotated `env.json`.

## Commands

| Command | What it does |
|---|---|
| `simmer hook <shell>` | Print the prompt hook for fish, bash, or zsh. |
| `simmer allow` / `simmer deny` | Trust or distrust the current project. |
| `simmer warm` | Build the environment now and wait for it (also allows the project). |
| `simmer export <shell>` | What the hook runs: print shell code that loads the environment. Waits for a rebuild when stale. |
| `simmer export json` | The whole environment as `{"NAME": "value" \| null}`, for programs (see [Headless callers](#headless-callers)). |
| `simmer status` | The current directory's state, as JSON. |
| `simmer doctor` | Timings, cache state, and configuration hazards, as JSON. |
| `simmer daemon` | Re-warm allowed projects as soon as a watched file changes (optional). |
| `simmer direnv …` | direnv-compatible CLI for editor plugins (see [Editors](#editors)). |

`simmer --help` and `simmer <command> --help` document every flag.

## While it builds

Commands that build (`warm`, and `export` when the environment is stale)
report progress on stderr:

- **On a terminal**, a live view redraws in place about 15 times a second:
  a header with the elapsed time and running/done/failed/queued counts, one
  line per running step (spinner, current activity such as the derivation
  nix is building, elapsed time) and the last few lines of its output. A
  finished step leaves a single `✓` (or `✗`, or `!` for a warning) line with
  its duration. Lines are cut to the terminal width, and a resize is picked
  up on the next frame.
- **Not on a terminal** (CI, pipes, `--plain`): one line per state change,
  no escape codes:

  ```text
  simmer: start   nix build
  simmer: ok      nix build (3.2s)
  simmer: start   provider GPU_ARCH
  simmer: warning provider GPU_ARCH (0.0s): `bash` exited with status 2; GPU_ARCH left unset
  simmer: warmed ~/src/app (42 vars, native, 3.7s)
  ```

- **`--json`**: one JSON object per event per line (`queued`, `start`,
  `phase`, `output`, `finish`, `note`, and a final `summary` with every
  step's status, duration and, for failures, the tail of its log). For
  `simmer warm --json` the events go to stdout.
- **`-q`** shows only failures; **`-v`** adds every output line, nix's
  activities, and a final summary table.

When a step fails, simmer prints that step's captured output (the last 40
lines; `--log-lines N` changes that, `--log-lines 0` prints all of it) and a
summary table of every step, and saves the full log in the project's cache
(the path is printed with the failure). Each step's output is held in a
bounded in-memory ring that spills to a temporary file, so a noisy build
cannot grow simmer's memory.

**Exit status** is 0 on success, the failing step's own exit status when a
step fails, 124 when `--timeout` expires, and 128+N when stopped by signal
N (130 for Ctrl-C).

**Ctrl-C / SIGTERM** stop every process the build started (its whole
process group: SIGTERM, then SIGKILL after a grace period), then print the
summary. Nothing is left running, and the next prompt does not start the
build again: it keeps the previous environment until you save a watched
file or run `simmer warm`.

**`--timeout 10m`** (or `SIMMER_TIMEOUT=10m`) bounds how long simmer waits;
the default is no limit.

**A failed build is not retried at every prompt.** simmer remembers that
this exact version failed, keeps the previous environment loaded, and says
so once per shell. Saving any watched file is a new version and is tried
again; `simmer warm` retries explicitly.

**Two shells, one project:** if a build is already running (another shell,
the daemon), the second one waits for it instead of starting its own.

**Prefer not to wait?** `simmer export --background` (or
`SIMMER_BACKGROUND=1` in your shell config) serves the previous environment
immediately and rebuilds in a detached process.

## Configuration

A project is the nearest directory (walking up) that contains any of
`flake.nix`, `.envrc`, `.simmer.toml`, or `simmer.toml`.

### `.simmer.toml`

Every key is optional; a bare flake project needs no manifest at all.
Unknown keys are errors, so a typo cannot silently do nothing.

```toml
# Which dev shell to load. Default: "." when flake.nix exists. A bare
# fragment (".#dev") means devShells.<your system>.dev.
flake = ".#dev"

# Extra files whose content defines the environment (flake.nix,
# flake.lock and the manifest itself are always watched). A wildcard entry
# is expanded at build time and its directory tree is watched by the
# daemon, so a file that appears later still triggers a rebuild.
watch = ["nix/**/*.nix"]

# KEY=value files to load (watched automatically).
dotenv = [".env.local"]

# Directories to prepend to PATH, relative to the project root.
path = ["bin", "scripts"]

# Literal variables.
[env]
DATABASE_URL = "postgres://localhost/dev"

# Commands whose stdout becomes a variable, run once per build in the
# finished environment. A failing or empty provider leaves its variable
# unset and is reported as a warning; it does not fail the build.
[[provider]]
env = "GPU_ARCH"
command = "nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d ."
invalidate = "boot"   # also re-run after a reboot; default "generation"
```

An annotated copy is in [`examples/simmer.toml`](examples/simmer.toml).

### `.envrc` (direnv compatibility)

An allowed `.envrc` runs in a scrubbed bash harness with a direnv-compatible
subset of its standard library: `use flake`, `use nix`, `PATH_add`,
`path_add`, `MANPATH_add`, `dotenv`, `dotenv_if_exists`, `source_env`,
`source_env_if_exists`, `source_up`, `watch_file` (feeds simmer's
invalidation), `env_vars_required`, `has`, `expand_path`, `log_status`,
`log_error`. `layout` is not supported (it warns). If `use flake` fails, the
build fails with nix's exit status. Trust for an `.envrc` project is bound
to the file's content: editing it revokes trust, as with direnv.

A `.simmer.toml` outranks a `.envrc`. With both present simmer never sources
the `.envrc` — it loads the project natively (or through the manifest) and
trusts it like any manifest project — while direnv-driven callers (a CI
builder's `direnv exec`) keep reading the `.envrc` unchanged. Keep the two
equivalent; editing the outranked `.envrc` does not rebuild simmer's
environment.

### `env.json` (environments as data)

A dev shell whose output contains an `env.json` is loaded *natively*: one
`nix build`, then the variables, providers and generated config files are
applied in Rust without sourcing any bash. The schema (closed: unknown
fields are errors) is documented in
[`crates/simmer-core/src/envspec.rs`](crates/simmer-core/src/envspec.rs);
[`examples/env.json`](examples/env.json) shows every field.

- `vars`: `set`, `prefix` (merged in front of the caller's value, like
  `PATH`), `unset`, `root` (the project root).
- `configs`: files linked, copied, or rendered from a template (`@VAR@`,
  `@VAR:-default@`) into the project at build time and listed in a marked
  block of `.gitignore`; the cache's GC root keeps their sources alive.
- `providers`: like the manifest's.
- `motd` and `commands` become the **entry banner**, printed to stderr when
  you enter the project (only on a terminal, never into a captured
  environment; `SIMMER_NO_MOTD=1` silences it).

A native build also roots every flake input next to the shell's GC root
(refreshed when `flake.lock` changes), so a rebuild after `nix store gc`
does not re-download sources.

### Environment variables

| Variable | Effect |
|---|---|
| `SIMMER_PROGRESS` | Default `--progress` mode: `auto`, `live`, `plain`, `json`, `quiet`. |
| `SIMMER_TIMEOUT` | Default `--timeout` (`90s`, `10m`, `1h`; `0` = no limit). |
| `SIMMER_BACKGROUND=1` | `export` rebuilds in the background instead of waiting. |
| `SIMMER_NO_MOTD=1` | Do not print entry banners. |
| `SIMMER_NO_HINTS=1` | Do not suggest `simmer allow` when entering unallowed projects. |
| `SIMMER_DIRENV_FALLBACK` | Path of the real direnv for the shim to delegate to. |
| `NO_COLOR` | Disable colours in the progress view. |
| `XDG_CACHE_HOME` | Cache location (default `~/.cache/simmer`). |

## Trust

Nothing is executed for a project until you allow it:

```sh
simmer allow    # or `simmer warm`, an explicit action that implies allow
simmer deny     # simmer goes inert here and stops hinting
```

For flake and manifest projects trust sticks to the directory (flake edits
are routine, and the daemon must be able to rebuild them). For `.envrc`
projects without a manifest it is bound to the file's content.

`simmer allow` is also the migration switch from direnv: until you allow a
project, simmer leaves it alone and direnv (if hooked) keeps owning it.

## Headless callers

A program that runs commands inside a project's environment (a CI builder)
asks for it as JSON and applies it to its own child processes:

```sh
simmer allow --dir "$ws"
simmer --flake 'path:.#ci' --no-pin-inputs \
  --nix-arg=--option --nix-arg=build-users-group --nix-arg= \
  --nix-arg=--override-input --nix-arg=dep --nix-arg="$mirror" \
  export json --dir "$ws"
```

`--flake` outranks the manifest's `flake`; every `--nix-arg` is appended
to each nix command the build runs, one argument per flag (empty ones
included). Both key the cache, so a profile built under one set is never
served under another. `--no-pin-inputs` skips GC-rooting the flake's
inputs, which only pays off for a cache that outlives the caller.
`export json` builds first when the environment is stale, and fails —
printing nothing — when the project is untrusted or the build fails.

## The daemon (optional)

```sh
simmer daemon                 # run in the foreground
# or as a systemd user service (Linux):
simmer daemon --print-systemd > ~/.config/systemd/user/simmer.service
systemctl --user enable --now simmer
```

The daemon watches every allowed project simmer has built and rebuilds it
as soon as a watched file changes, so the multi-second evaluation happens
when you save, and the next prompt usually finds the environment ready.
It has no socket or protocol: the on-disk cache is the interface.

## Editors

Editor plugins (VS Code, Zed, Emacs, …) and launchers call `direnv export
json` or `direnv exec`. Install the `direnv`-named shim ahead of the real
direnv on `PATH`:

```sh
nix profile install github:seryl/simmer#direnv-shim
```

For projects you have allowed, the shim answers from simmer (waiting for a
rebuild the way direnv waits for an `.envrc`); everything else is passed to
the real direnv (`$SIMMER_DIRENV_FALLBACK`, or the next `direnv` on `PATH`).
Without a real direnv, the shim is the only loader.

## Performance

Measured on an Apple Silicon laptop (macOS, 2026-10-12) with
[hyperfine](https://github.com/sharkdp/hyperfine), 2000 runs each,
interleaved, for a project with 23 watched files and a 40-variable
environment. `/usr/bin/true` is the cost of starting any process at all.

| Path | 0.1.0 (previous) | now | `/usr/bin/true` |
|---|---|---|---|
| Prompt, environment current (`simmer export fish`) | 5.1 ms | 3.1 ms | 1.6 ms |
| Entering the project (apply script) | 5.2 ms | 2.9 ms | |

The in-process work on the prompt path is about 0.14 ms; the rest is
process start-up. That is why the prompt binary links nothing but the
system C library: the daemon, which needs the platform's file-watching
frameworks, is a separate binary.

## How it works

- **Build** (rare: a watched file changed). Either *native* — `nix build`
  the dev shell and apply its `env.json` — or *harness* — run
  `nix print-dev-env` and the shell hook (or the project's `.envrc`) in a
  bash harness with a scrubbed environment, and capture what comes out.
  The result is stored with a GC root that pins its closure.
- **Export** (every prompt). Fingerprint the watched files by stat (mtime,
  size, inode); hash their content only if a fingerprint changed; compare
  with the cached version and with the version the shell already holds.
  Emit nothing, or a native script that applies the difference.
- Session state travels inside the shell (`__SIMMER_STATE`), so leaving a
  project restores exactly what was there before, without reading the cache.
- `PATH` and `XDG_DATA_DIRS` are stored as "nix part + your current value"
  and merged against your *current* value at export time.

## Troubleshooting

- `simmer doctor` explains what simmer sees for the current directory:
  whether it is trusted, what is watched, the last build and its failure,
  and how long the prompt path takes.
- `simmer status` is the same state in a smaller, stable JSON shape.
- A build failed: the full log of the failing step is saved in the
  project's cache (the path is printed with the failure); `simmer warm -v`
  shows everything as it happens.
- Nothing happens in a project: is it allowed (`simmer allow`)? Is the hook
  installed (`type __simmer_hook` in your shell)?

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Changes are listed in
[CHANGELOG.md](CHANGELOG.md).

## License

Licensed under the [MIT license](LICENSE). Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in simmer
by you shall be licensed as above, without any additional terms or
conditions.
