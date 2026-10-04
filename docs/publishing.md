# Publishing a release

The current published release is **0.0.2**, a documentation and metadata update with unchanged runtime behavior and wire formats. The project remains experimental and pre-1.0. Preparing packages and running dry-runs does not upload them.

The examples below use `0.0.2` to show a complete release sequence. **For a future release, choose an unpublished version** and update the workspace version, exact companion requirements, installation examples, and changelog together before running upload commands. Published versions cannot be overwritten.

## Release scope

The shared version lives in `[workspace.package]` in the root `Cargo.toml`. Every member inherits it; internal runtime dependencies use `=0.0.2` plus their local paths. Keep those requirements synchronized whenever the release version changes. During the `0.0.x` series, pin exact versions and expect API changes; there is no 1.0 compatibility guarantee.

Publish these packages in dependency order:

1. `bevy_agent_core`
2. `bevy_agent_snapshot`
3. `bevy_agent_replay`
4. `bevy_agent_runner`
5. `bevy_agent_remote`
6. `bevy_agent_cli` (independent of the runtime crates)

`bevy_agent_cli` installs the **`agentctl`** executable. The crates.io name `agentctl` is already used by another project. All six packages are already registered to this project. Verify ownership and the next version before uploading; published versions cannot be overwritten.

`sample_platformer` has `publish = false`. It remains a repository example and regression suite. The Python client, repository documentation, agent skills, and fuzz harness are also distributed through Git, not through the Rust crate archives. The runner's complete `counter` example is included in its crate archive.

## Validate from the canonical checkout

Require Rust 1.91 or newer. CI uses Rust 1.91.0; an installed 1.91.x toolchain can validate the minimum supported version locally. Avoid upgrading unrelated dependencies just to update local package versions.

```sh
cargo fmt --all -- --check
cargo fmt --manifest-path fuzz/Cargo.toml -- --check
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
python3 -m unittest discover -s python/tests
cargo run -p bevy_agent_runner --example counter --locked
cargo run -p bevy_agent_cli --bin agentctl --locked -- --help
cargo run -p bevy_agent_cli --bin agentctl --locked -- --version
```

Run the all-feature tests and rendered capture smoke from the existing manual CI workflow when validating visual runtime behavior. Linux needs the Bevy development libraries installed by that job. For transport validation, build and run the existing smoke script:

```sh
cargo build --workspace --bins --examples --locked
python3 scripts/smoke.py
```

On the pinned personal Mac mini, first run **`build-storage-check`**, then replace native `cargo` build, test, run, lint, doc, and package commands above with **`cargo-storage`** from the canonical repository root. For example:

```sh
build-storage-check
cargo-storage +1.91.1 test --workspace --all-targets --locked
cargo-storage +1.91.1 test --workspace --doc --locked
cargo-storage +1.91.1 package --workspace --exclude sample_platformer --locked
```

Use an installed toolchain. Keep CI's normal Cargo commands. The guarded launcher reports its output directory and refuses insufficient storage; do not redirect output or fall back to an internal target. Unique evidence and release archives stay outside the generated build pool under the existing host policy.

When running `scripts/smoke.py` after a guarded build, pass `--bin-dir` with the launcher's reported output directory followed by `/debug`. The script's default assumes ordinary Cargo output at the repository's `target/debug`.

## Inspect and verify the archives

Cargo's workspace packaging prepares the selected local crates together, allowing their versioned path dependencies to resolve during package verification before the first upload.

```sh
cargo package --workspace --exclude sample_platformer --list --locked
cargo package --workspace --exclude sample_platformer --locked
```

For inspection while changes are uncommitted, append `--allow-dirty`. Final release verification must run from a clean, committed checkout so Cargo records the intended source revision. `--no-verify` only creates archives; it does **not** prove the packaged source builds. See the [Cargo packaging reference](https://doc.rust-lang.org/cargo/commands/cargo-package.html).

Check each generated `.crate` archive:

- The normalized manifest has version `0.0.2`, a description, repository/homepage, license expression, keywords/categories, and the correct package README.
- Internal dependencies have registry-compatible exact versions. Normalized manifests do not rely on workspace inheritance or local `path` dependencies.
- Both `LICENSE-MIT` and `LICENSE-APACHE` are present and match the canonical root licenses.
- Library archives include their README (also used as crate-level rustdoc), source, and any packaged examples/tests. The runner includes `examples/counter.rs`.
- Captures, replay output, build caches, Python bytecode, credentials, and coverage output are absent.
- docs.rs metadata enables all features for each library. Validate with rustdoc warnings denied; see [docs.rs metadata](https://docs.rs/about/metadata).

Explicit `include` rules keep each package scoped. Root license texts are copied into the package directories; update all copies if those texts change.

## Upload only when releasing

Authenticate with a crates.io account that owns these names. The token needs **publish-update** permission for the six `bevy_agent_*` packages; a **publish-new** token only registers new packages. Use `cargo login` or the standard credential provider; do not put a token in the repository. Mark `0.0.2` released in `CHANGELOG.md`, update the README release status and installation wording, commit the release changes, and finish the clean-checkout validation above.

Dry-run and publish each package in order:

```sh
cargo publish -p bevy_agent_core --locked --dry-run
cargo publish -p bevy_agent_core --locked
# Wait until bevy_agent_core 0.0.2 appears in the registry index.

cargo publish -p bevy_agent_snapshot --locked --dry-run
cargo publish -p bevy_agent_snapshot --locked
# Wait until bevy_agent_snapshot 0.0.2 is indexed.

cargo publish -p bevy_agent_replay --locked --dry-run
cargo publish -p bevy_agent_replay --locked
# Wait until bevy_agent_replay 0.0.2 is indexed.

cargo publish -p bevy_agent_runner --locked --dry-run
cargo publish -p bevy_agent_runner --locked
# Wait until bevy_agent_runner 0.0.2 is indexed.

cargo publish -p bevy_agent_remote --locked --dry-run
cargo publish -p bevy_agent_remote --locked

cargo publish -p bevy_agent_cli --locked --dry-run
cargo publish -p bevy_agent_cli --locked
```

Individual downstream dry-runs need their runtime dependencies in the registry. Workspace package verification proves the local release set builds, but it does not prove registry ownership, credential validity, or a successful upload. The independent core and CLI can be dry-run before any internal dependency is published.

After upload, verify each crates.io page and docs.rs build. Test a fresh consumer using only registry dependencies, and run `cargo install bevy_agent_cli --version 0.0.2 --locked` followed by `agentctl --version`. Tag the verified release as `v0.0.2` and link the changelog. A package version cannot be overwritten after publication; any source fix needs a new release version.

## Artifact compatibility

Cargo package version `0.0.2` does not reset protocol or file formats. Snapshots, replay manifests, and replay bundles use format version **3**. Stable gameplay type IDs, per-type schema versions, and checksum encoding versions have their own compatibility rules. Older artifacts are rejected; this release does not migrate them.
