# simmer

Instant Nix devshells: evaluate once, keep it simmering, replay natively
into fish/bash/zsh in microseconds.

Replaces the direnv + nix-direnv + devshell-framework load path for flake
projects. Design rationale and the measurements that motivated it:
`the design notes`.

## How it works

Two paths, nothing else:

- **Warm** (rare — a watched file changed): `nix print-dev-env --json`,
  run the shellHook once in a scrubbed bash harness, run declared
  providers, diff the result against a deterministic baseline, cache the
  final environment keyed by a content hash of the watched files. A
  `--profile` GC root pins the closure.
- **Export** (every prompt): compare generation hashes, emit native shell
  code only when something changed. No bash, no nix, no subprocesses.
  Measured ~0.2ms.

Environments from any flake devShell work unchanged (`mkShell`, numtide
devshell, std) because everything flows through `print-dev-env`. Caching
is content-hash based and deliberately independent of Nix's eval cache,
which dirty git trees silently disable.

The daemon has **no IPC**: the disk cache is the interface. It watches
registered projects (anything you've warmed) and re-warms the moment a
watched file changes — the multi-second eval happens when you save
flake.nix, not when you next open a shell. Meanwhile `export` serves the
previous environment rather than ever blocking a prompt.

## Use

```fish
# fish config:
simmer hook fish | source
```

```bash
# bash/zsh config:
eval "$(simmer hook bash)"   # or zsh

# CI / one-off:
eval "$(simmer export bash)"

# pre-warm daemon (systemd user service):
simmer daemon --print-systemd > ~/.config/systemd/user/simmer.service
systemctl --user enable --now simmer

# diagnose the load path:
simmer doctor
```

Do not run direnv's nix integration and the simmer hook on the same
project — two systems fighting over the same variables. `simmer doctor`
warns about this.

## simmer.toml

```toml
# Files beyond flake.{nix,lock} whose content defines the env. Flakes
# that import their shell from another file are silently stale otherwise.
watch = ["nix/shells/dev.nix"]

# Declarative replacements for imperative shellHook probing: runs once at
# warm time in the post-hook environment, stdout becomes an exported var.
[[provider]]
env = "CUDA_COMPUTE_CAP"
command = "nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d ."
invalidate = "boot"   # re-run after reboot; default is "generation"
```

## Measured (dev machine, 2026-08-30)

| Path | direnv + nix-direnv | simmer |
|---|---|---|
| Prompt, env current | 350–700 ms | **~0.2 ms** |
| Enter project, cache warm | 350–700 ms | ~15 ms |
| Watched file changed | 10–16 s, blocks next entry | daemon pre-warms on save; prompt never blocks |

## Layout

- `crates/simmer-core` — library: env spec parsing, warm harness, profile
  diffing, per-shell compilers, cache, registry, config.
- `crates/simmer` — CLI: `export`, `warm`, `hook`, `status`, `daemon`,
  `doctor`.

## Design notes

- The cached artifact is the **post-shellHook** environment: frameworks
  emit sentinels (`PATH=/path-not-set`) in the JSON and build the real
  env imperatively in the hook, so only the hook's output is worth
  replaying.
- The warm harness scrubs its inherited environment (HOME/PATH/TERM/LANG
  only) so profiles are deterministic wherever the warm ran.
- Session state (saved variables, set list) rides in `__SIMMER_STATE`
  inside the shell itself, so unload needs no cache access and survives
  cache GCs.
- `PATH`/`XDG_DATA_DIRS` are stored as nix-prefix + caller-tail and
  re-merged against the *current* caller value at export time.

## Roadmap

- Nix-side flake module emitting `env.json` directly (skip the
  `print-dev-env` bash round trip; menus/commands as data).
- Provider outputs beyond env vars (generated files with content-diff
  writes).
- macOS boot-scoped invalidation (`kern.boottime`).
