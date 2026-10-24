# The CARGO_PKG_* values Cargo would set, for macros such as clap's `version`.
# VERSION must match [workspace.package] version in Cargo.toml; the
# //:cargo-version-check test fails when they drift.

VERSION = "0.1.0"

def cargo_env(name, description):
    major, minor, patch = VERSION.split(".")
    return {
        "CARGO_CRATE_NAME": name.replace("-", "_"),
        "CARGO_PKG_AUTHORS": "Josh Toft <joshtoft@gmail.com>",
        "CARGO_PKG_DESCRIPTION": description,
        "CARGO_PKG_NAME": name,
        "CARGO_PKG_REPOSITORY": "https://github.com/seryl/simmer",
        "CARGO_PKG_VERSION": VERSION,
        "CARGO_PKG_VERSION_MAJOR": major,
        "CARGO_PKG_VERSION_MINOR": minor,
        "CARGO_PKG_VERSION_PATCH": patch,
        "CARGO_PKG_VERSION_PRE": "",
    }
