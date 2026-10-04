# Contributing and docs

The documentation lives beside the Rust source so examples, contracts, and package links can be reviewed together.

## Develop the guide

Use Node 24 and npm. From the repository root:

```sh
npm ci
npm run docs:dev
```

The guide uses VitePress, local search, self-hosted fonts, and a small Vue walkthrough. Getting started includes the packaged Rust counter source rather than a duplicate code sample.

## Verify a production build

```sh
npm run docs:build
npm run docs:check
npm run docs:preview
```

Check the production preview at the reported `/bevy-agent/` path. Verify both desktop and mobile navigation, search, code copying, dark mode, and the counter's save/restore loop.

The manually dispatched **Documentation** workflow builds and uploads the static site to GitHub Pages. Its public guide is `https://briansunter.github.io/bevy-agent/`. Rust CI remains separately dispatched; a docs build does not prove runtime correctness.

## Validate runtime changes

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python3 -m unittest discover -s python/tests
```

Use the existing manual CI workflow for all-feature tests, transport smoke, fuzzing, and rendered capture. [Publishing](../publishing.md) adds package archive checks and registry acceptance.

## Personal Mac mini

On the pinned personal Mac mini, run `build-storage-check` before the native build sequence. Use `cargo-storage` from the canonical worktree instead of Cargo for build, run, test, lint, doc, package, and install commands:

```sh
build-storage-check
cargo-storage +1.91.1 run -p bevy_agent_runner --example counter --locked
```

The supported launcher supplies generated/intermediate output placement and checks storage. Do not add competing output paths or fall back internally after refusal. Other hosts and CI retain their normal Cargo commands.
