#!/usr/bin/env bash
# Usage: check-cargo-version.sh <expected-version> <Cargo.toml>
set -euo pipefail
expected=$1
actual=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$2")
if [[ $actual != "$expected" ]]; then
  echo "cargo_env.bzl VERSION is $expected but Cargo.toml says $actual" >&2
  exit 1
fi
