# Publishing Checklist

This workspace is split into publishable crates. Publish in dependency order:

1. `bevy_agent_core`
2. `bevy_agent_snapshot`
3. `bevy_agent_replay`
4. `bevy_agent_runner`
5. `bevy_agent_remote`
6. `sample_platformer` and `agentctl` if you want the example game and CLI on crates.io

Before publishing:

```sh
cargo fmt --all --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo llvm-cov --workspace --all-targets --all-features --summary-only
cargo package --workspace --no-verify
```

Use `--dry-run` for each crate before publishing. Because these crates depend on each other by registry version, downstream dry-runs only pass after their dependencies have actually been published and indexed:

```sh
cargo publish -p bevy_agent_core --dry-run
# Publish bevy_agent_core, wait for the index, then dry-run/publish the next crate.
cargo publish -p bevy_agent_snapshot --dry-run
cargo publish -p bevy_agent_replay --dry-run
cargo publish -p bevy_agent_runner --dry-run
cargo publish -p bevy_agent_remote --dry-run
```

Notes:

- The manifests use versioned local path dependencies so packaged crates carry registry-compatible dependency requirements.
- The workspace is dual-licensed under `MIT OR Apache-2.0`.
- `sample_platformer` is useful for examples and integration tests, but applications can depend only on the core/runtime crates.
- Do not publish with test artifacts, local replay files, or generated coverage outputs in the package.
