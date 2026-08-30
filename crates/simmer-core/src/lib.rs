//! # simmer-core
//!
//! Devshell environment engine. The pipeline has exactly two paths:
//!
//! - **Warm** (slow, rare — flake inputs changed): run
//!   `nix print-dev-env --json`, execute the shellHook once inside a bash
//!   harness, capture the environment it produces, and store the result as
//!   an [`profile::EnvProfile`] keyed by a content hash of the project's
//!   watched files.
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
//! changed.

pub mod cache;
pub mod compile;
pub mod config;
pub mod envspec;
pub mod profile;
pub mod project;
pub mod registry;
pub mod warm;
