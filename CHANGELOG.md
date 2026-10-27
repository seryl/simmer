# Changelog

All notable changes to simmer are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/).

## Unreleased

### Added

- **Headless callers.** `simmer export json` prints the whole environment
  as `{"NAME": "value" | null}` for a program to apply, building first
  when stale; an untrusted project or a failed build is an error. Global
  `--flake <REF>` outranks the manifest's `flake`, and repeatable
  `--nix-arg <ARG>` appends an argument to every nix command the build
  runs; both key the cache. `--no-pin-inputs` skips rooting flake inputs
  for a cache that dies with its caller. Together they replace a CI
  builder's `direnv export json` over a `.envrc`.
- **A warm fetches the way its caller does.** nix's own settings
  (`NIX_CONFIG`, `NIX_REMOTE`, `NIX_SSL_CERT_FILE`, …), TLS trust,
  proxies and git's environment config (`GIT_CONFIG_COUNT` /
  `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`) now reach nix; a CI builder's
  substituters and mirror credential previously never did. Like the
  rest of the caller's environment they are never recorded in a profile.

### Changed

- **A `.simmer.toml` outranks a `.envrc`.** A project with both loads
  natively (or through the manifest) and simmer never sources the
  `.envrc`, which stays for direnv-driven callers such as a CI builder.
  Trust for such a project sticks to the directory, and an `.envrc`
  edit no longer invalidates the cache. Warms that ran the `.envrc`
  record mode `envrc` rather than `harness`, so the first warm after a
  manifest appears probes for a native shell.

- **simmer waits instead of backgrounding.** When the environment is stale,
  `simmer export` (the prompt hook) now rebuilds in the foreground and then
  applies the fresh environment, instead of serving the old one and
  rebuilding in a detached process. `--background` (or
  `SIMMER_BACKGROUND=1`) restores the old behaviour. The direnv shim waits
  the same way.
- A second `simmer warm` for the same project waits for the one already
  running instead of returning immediately. The warm lock is now a kernel
  `flock` (released automatically if a warm crashes).
- A failing step's exit status is simmer's exit status (124 on timeout,
  128+N when stopped by signal N). Failures are no longer silent: failing
  providers and input pinning are reported as warnings with their output,
  and `use flake` / `use nix` in an `.envrc` fail the build when nix fails
  (they used to produce an environment without the dev shell).
- A version that failed to build is not rebuilt at every prompt; the
  previous environment stays loaded and simmer says so once per shell.
- The pre-warm daemon is a separate `simmer-daemon` binary (`simmer daemon`
  runs it), so the prompt-time binary no longer links CoreServices and
  CoreFoundation on macOS.
- The zsh hook also runs before each prompt (`precmd`), like fish and bash,
  so edits are picked up without a `cd`.

### Added

- A progress display for everything simmer waits on: a live, in-place view
  on terminals (header with counts, one line per running step with its
  phase and output tail, `✓`/`✗` lines for finished steps), plain lines for
  CI (`--plain`), JSON events (`--json`), `-q` and `-v`. Failed steps print
  their log (`--log-lines`) and a summary table; full logs are saved in the
  project's cache. `NO_COLOR` is respected.
- `--timeout` / `SIMMER_TIMEOUT` to bound how long a build may take.
- Ctrl-C and SIGTERM stop every process a build started, then report; an
  interrupted rebuild is not restarted by the next prompt. The daemon stops
  its children the same way when it is stopped.
- `simmer status` and `simmer doctor` report the last failed build.
- A one-time hint when entering a project that is not allowed yet
  (`SIMMER_NO_HINTS=1` silences it; `simmer deny` stops it per project).
- `examples/` with a small flake, an annotated manifest and `env.json`.

### Fixed

- A `.simmer.toml` that does not parse is reported (at every prompt, by
  `status`, `warm`, `allow`) instead of making the project look absent,
  which silently unloaded its environment.
- A build that creates a watched file (nix writes `flake.lock` on a flake's
  first evaluation) no longer leaves the environment stale at once, which
  caused an immediate second rebuild.
- A prompt-time rebuild no longer re-probes for a native `env.json` shell
  (an extra `nix build`) when the last build already settled on the bash
  harness; `simmer warm` still probes.
- Multi-line nix errors keep their line breaks in logs, and a failed
  harness build is summarised by nix's own `error:` line.

- The daemon's project registry no longer drops `.envrc`-only and
  manifest-only projects.
- Watches on directories declared by an `.envrc` are detected relative to
  the project root rather than simmer's working directory.

### Performance

- The prompt path (`simmer export`, environment current) went from 5.1 ms
  to 3.1 ms on macOS, and entering a project from 5.2 ms to 2.9 ms, mostly
  by no longer loading the file-watching frameworks and libiconv at
  start-up. The cache directory is no longer created on every prompt;
  template config files are rendered once per build instead of three
  times; flake inputs are pinned in parallel with materializing the
  environment; prompt-time rebuilds of harness projects skip the
  speculative native `nix build`.

## 0.1.0

Initial version: cached Nix dev environments replayed into fish, bash and
zsh; native `env.json` warm mode and the bash harness for `.envrc` and
other flakes; trust; the pre-warm daemon; the direnv-compatible shim.
