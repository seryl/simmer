//! # simmer-core
//!
//! Devshell environment engine. The pipeline has exactly two paths:
//!
//! - **Warm** (slow, rare — flake inputs changed): build the environment
//!   once and store it as an [`profile::EnvProfile`] keyed by a content
//!   hash of the project's watched files. Shells that export an
//!   `env.json` ([`envspec::EnvSpec`]) are materialized in Rust from one
//!   `nix build`; anything else runs `nix print-dev-env` plus its
//!   shellHook inside a bash harness and captures what comes out.
//! - **Export** (fast, every shell entry): load the cached profile and
//!   compile it to native shell code (fish/bash/zsh). No bash is sourced,
//!   no process is spawned, no nix runs. A generation guard makes the
//!   no-change case a single string comparison.
//!
//! Caching the *post-hook* environment is the load-bearing decision: shell
//! frameworks (numtide devshell, std) emit sentinel values
//! (`PATH=/path-not-set`, `HOME=/homeless-shelter`) in the JSON and build
//! the real environment imperatively in the hook, so only the hook's
//! output is worth replaying. Hook side effects (generated config files)
//! happen at warm time, which is also the only time their inputs can have
//! changed — and a native shell declares them as data ([`configs`]), so
//! they are linked with a GC root instead of planted by a hook.

pub mod cache;
pub mod compile;
pub mod config;
pub mod configs;
pub mod envspec;
pub mod menu;
pub mod nixlog;
pub mod profile;
pub mod progress;
pub mod project;
pub mod registry;
pub mod supervise;
pub mod term;
pub mod warm;
