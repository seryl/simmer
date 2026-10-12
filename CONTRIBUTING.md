# Contributing to simmer

Thanks for helping. Bug reports, fixes, and focused features are welcome.

## Getting set up

You need [Nix](https://nixos.org/download) with flakes enabled. The dev
shell has the pinned Rust toolchain (cargo, clippy, rustfmt,
rust-analyzer):

```sh
nix develop            # or: simmer allow, if you already use simmer
cargo build
cargo test
```

The full check suite is a flake check:

```sh
nix flake check        # package build, rustfmt, clippy, tests
```

or piecewise:

```sh
cargo fmt --check
cargo clippy --all-targets -- --deny warnings
cargo test
```

## Layout

- `crates/simmer-core` — the library: project discovery and generation
  hashing (`project`), the cache (`cache`), the build (`warm`, with
  `shim.bash` for the `.envrc` harness), `env.json` (`envspec`, `configs`,
  `menu`), compiling an environment to shell code (`compile`), running and
  waiting for child processes (`supervise`), and the progress display
  (`progress`, `term`, `nixlog`).
- `crates/simmer` — the `simmer` CLI (`main.rs`), the direnv-compatible
  shim (`direnv_shim.rs`), and the `simmer-daemon` binary
  (`src/bin/simmer-daemon.rs`).
- `crates/simmer/tests/` — end-to-end tests that drive the binary against
  a stand-in `nix` (`tests/support/nix`).
- `examples/` — documented examples; unit tests check that they parse.

## Ground rules

- **The prompt path stays cheap.** `simmer export` with a current
  environment must not spawn processes, parse large files, or allocate in
  proportion to anything unbounded. If you touch it, measure before and
  after (see the README's Performance section for the method).
- **Waiting is the contract.** Anything that starts a process goes through
  `supervise::run`, reports to a `Progress`, and returns only after the
  process has exited. No fire-and-forget threads or detached children
  unless the user asked for it explicitly (like `--background`).
- **Bounded memory.** Output from children is kept in bounded buffers;
  never collect an unbounded stream into memory.
- **No `unwrap`/`expect` outside tests.** Return errors with context
  (`anyhow::Context`) that tells the user what to do next.
- **Tests are deterministic.** Prefer pure functions with injected time and
  sizes (see `progress/tests.rs`) over sleeps. When a test must wait for
  another process, poll for the condition with a generous upper bound and
  fail, never hang.
- **Original work only.** Do not paste code from other projects. Every
  dependency must have a permissive license (MIT, Apache-2.0, BSD, ISC,
  zlib, CC0, Unicode, Unlicense); no copyleft.

## Submitting changes

1. Open an issue first for anything larger than a fix, so the approach can
   be agreed before you spend time on it.
2. Keep commits small and focused, with messages that say *why*.
3. Add or update tests, and add a line to `CHANGELOG.md` under
   "Unreleased".
4. Make sure `nix flake check` (or the three cargo commands above) passes.

By contributing you agree that your contributions are licensed under the
MIT license, as described in the README.
