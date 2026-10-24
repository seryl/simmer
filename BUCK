load("//:cargo_env.bzl", "VERSION")

# Fails when cargo_env.bzl's VERSION drifts from the workspace version.
sh_test(
    name = "cargo-version-check",
    test = "scripts/check-cargo-version.sh",
    args = [VERSION, "$(location :Cargo.toml)"],
)

export_file(
    name = "Cargo.toml",
    visibility = ["PUBLIC"],
)
