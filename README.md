# simmer

Instant Nix devshells: evaluate once, keep it simmering, replay natively
into fish/bash/zsh in microseconds.

A standalone replacement for the direnv + nix-direnv load path. Projects
declare themselves with a `.simmer.toml` (preferred) or keep their
existing direnv `.envrc` — both run through the same engine. Platform
design (including where this is headed beyond shells):
`the design notes`.

## How it works

Two paths, nothing else:

- **Warm** (rare — a watched file changed): build the environment once
  and cache the captured result, keyed by a content hash of the watched
  files. A `--profile` GC root pins the closure. Two ways to build it:
  - **native** — the devshell derivation ships an `env.json` (see
    below): one `nix build`, then the variables, providers and generated
    config files are materialized in Rust. No bash is sourced.
  - **harness** — any other flake shell or a `.envrc`: `use_flake` evals
    `nix print-dev-env` output in a scrubbed bash harness, then
    dotenv/PATH/env lines from the manifest, then providers.
  An explicit `simmer warm` probes native first and remembers the answer;
  the daemon reuses it.
- **Export** (every prompt): compare generation hashes, emit native shell
  code only when something changed. No bash, no nix, no subprocesses;
  measured well under a millisecond. The watched files are fingerprinted
  by stat (mtime, size, inode); their content is hashed only after a
  fingerprint changes, so dozens of watched files cost microseconds.

The daemon has **no IPC**: the disk cache is the interface. It re-warms
registered projects the moment a watched file changes, so the
multi-second eval happens when you *save*, not when you next open a
shell; meanwhile `export` serves the previous environment rather than
ever blocking a prompt.

## `env.json` — environments as data

A devshell that exports an `env.json` next to its profile is warmed
without running any of its bash. The schema (`simmer_core::envspec`,
closed — unknown fields are errors):

```json
{ "schema": 1, "name": "dev", "system": "x86_64-linux",
  "profile": "/nix/store/...-dev-profile",
  "vars": { "PATH": { "kind": "prefix", "value": "/nix/store/...-dev-profile/bin" },
            "PRJ_ROOT": { "kind": "root" }, "FOO": { "kind": "set", "value": "1" } },
  "commands": [ { "name": "fmt", "help": "format", "category": "code",
                  "run": { "kind": "script", "source": "/nix/store/.../bin/fmt" } } ],
  "configs":  [ { "output": ".rustfmt.toml", "source": "/nix/store/...", "mode": "link" } ],
  "providers": [ { "env": "CUDA_COMPUTE_CAP", "command": "nvidia-smi ...", "invalidate": "boot" } ] }
```

`vars` kinds: `set`, `prefix` (merged in front of the caller's value),
`unset`, `root` (the project root, never stored). `configs` are linked
(or copied, or template-substituted with `@VAR@` / `@VAR:-default@`) into
the project at warm time and listed in a marked block of `.gitignore`;
the GC root keeps their sources alive, so a linked config can no longer
dangle after `nix store gc`. `providers` declared here run like manifest providers. `motd` and the
`commands` list become the **entry banner**: rendered once at warm time,
printed to stderr (only when stderr is a terminal) the first time a
session enters the project — never on a generation switch, never into a
captured environment. `SIMMER_NO_MOTD=1` silences it; `a project menu command`
prints the same listing on demand.

A native warm also roots every **flake input** next to the shell's GC
root (`inputs/` in the project's cache directory, refreshed when
`flake.lock` changes), so a warm after `nix store gc` re-evaluates from
the store instead of re-fetching sources.
The buildenv framework (`a consuming project`) emits this
from a pure-data shell declaration.

## Trust

Nothing is executed or applied for a project until you allow it:

```bash
simmer allow    # or: simmer warm (implicit allow, it's an explicit action)
simmer deny     # revoke; simmer goes inert for the project
```

For `.envrc` projects trust is bound to the file's content — editing it
revokes trust, exactly like direnv. For flake/manifest projects trust
sticks to the root (flake edits are routine; the daemon must be able to
pre-warm them).

`simmer allow` is also the **migration switch**: until you allow a
project, simmer stays inert and direnv (if hooked) keeps owning it. No
repo changes, reversible per project.

## `.simmer.toml` (preferred)

```toml
# Which environment to load; defaults to "." when flake.nix exists, so a
# bare flake project needs no manifest at all.
flake = ".#dev"

# Extra files whose content defines the env (shells imported from other
# files are silently stale without this). A wildcard entry is expanded
# at warm time and its directory tree is watched by the daemon, so a
# file that appears later still triggers a re-warm; the prompt never
# walks the tree.
watch = ["nix/**/*.nix"]

# Declarative equivalents of the common .envrc lines:
dotenv = [".env.local"]
path = ["bin", "scripts"]
[env]
DATABASE_URL = "postgres://localhost/dev"

# Commands whose stdout becomes an exported variable, run once at warm
# time in the finished environment.
[[provider]]
env = "CUDA_COMPUTE_CAP"
command = "nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d ."
invalidate = "boot"   # re-run after reboot; default is "generation"
```

## `.envrc` compatibility

An allowed `.envrc` runs in the warm harness against a direnv-compatible
stdlib subset: `use flake` / `use nix`, `PATH_add`, `dotenv`,
`source_env`, `source_up`, `watch_file` (feeds simmer's invalidation),
`env_vars_required`, and friends. `layout` is unsupported (loud warning).
The repo needs no changes; teammates keep using direnv against the same
file.

The `direnv`-named shim (`packages.direnv-shim`) serves editor plugins
and `direnv exec` callers. With a real direnv installed it delegates
projects simmer does not manage to it; without one it is the only
loader — `export` for such a project prints nothing and `exec` runs the
command in the caller's environment.

## Use

```fish
# fish config:
simmer hook fish | source
```

```bash
# bash/zsh config:
eval "$(simmer hook bash)"     # or zsh

# CI / one-off:
eval "$(simmer export bash)"

# pre-warm daemon (systemd user service):
simmer daemon --print-systemd > ~/.config/systemd/user/simmer.service
systemctl --user enable --now simmer

# diagnose the load path:
simmer doctor
```

## Measured (dev machine, 2026-08-30/31)

| Path | direnv + nix-direnv | simmer |
|---|---|---|
| Prompt, env current | 4 ms | **~0.2 ms** (cheaper than spawning `/bin/true`) |
| Entering a project | 110–700 ms load event | ~2 ms |
| Watched file changed | 10–16 s, blocks next entry | daemon pre-warms on save; prompt never blocks |

## Layout

- `crates/simmer-core` — library: warm (native env.json + bash harness
  and shim), envspec, configs, profile diffing, per-shell compilers,
  cache, trust, registry, manifest.
- `crates/simmer` — CLI: `export`, `warm`, `hook`, `status`, `daemon`,
  `doctor`, `allow`, `deny`.

## Design notes

- The cached artifact is the **post-hook** environment; frameworks emit
  sentinels (`PATH=/path-not-set`) and build the real env imperatively,
  so only the finished product is worth replaying.
- The warm harness scrubs its inherited environment (HOME/PATH/TERM/LANG
  only) so profiles are deterministic wherever the warm ran.
- Session state rides in `__SIMMER_STATE` inside the shell itself;
  unload needs no cache access.
- `PATH`/`XDG_DATA_DIRS` are stored as nix-prefix + caller-tail and
  re-merged against the *current* caller value at export time.
- Generations are content hashes over static ∪ dynamic watches —
  deliberately independent of Nix's eval cache, which dirty git trees
  silently disable.

## Roadmap

- `nix-simmer`: a `use simmer` library for people staying on direnv —
  shared cache, no more blocking reloads.
- `simmer-deploy`: the same kernel at host scope (pull-based NixOS
  reconciliation with pre-built generations and health-gated rollback).
- Done: `env.json` native warm mode (2026-09-10); the flake side lives in
  buildenv.
