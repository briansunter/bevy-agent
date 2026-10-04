# Snapshots and replay

Use a snapshot when you want to return to a known state. Use replay when you want to reproduce a sequence. Use a branch when you want to compare decisions from the same earlier state.

## Save and restore a state

```rust
let saved = env.snapshot()?;
let first = env.step(AgentAction::Noop)?;
env.restore(saved.snapshot_id)?;
let repeated = env.step(AgentAction::Noop)?;
assert_eq!(first.checksum, repeated.checksum);
```

This excerpt runs inside an initialized `AgentApp`. The [getting-started example](../getting-started.md) includes every required registration.

CLI equivalents:

```sh
agentctl snapshot
agentctl snapshots
agentctl restore <snapshot-id>
```

Replace `<snapshot-id>` with the identifier returned by the server. Required gameplay resources must be registered as required snapshot resources; otherwise missing state may make later gameplay invalid.

## Return to a tick

```sh
agentctl restore-tick 10
```

The runner finds a compatible checkpoint and reconstructs the state using recorded input. Pending actions, branch ancestry, and the checkpoint's visibility participate in reconstruction.

You can only navigate ticks supported by the retained history. Snapshot and replay owners each default to a configurable 64 MiB retention budget; they do not retain unlimited episodes.

## Try an alternate decision

```sh
agentctl branch --from-tick 10 --label try_jump
agentctl step '{"type":"Jump"}'
```

This example assumes the current environment supports `Jump` and has history at tick 10. Branches preserve lineage rather than pretending two incompatible futures are one timeline.

## Export a portable bundle

Start the sample server with an artifact root:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

Then:

```sh
agentctl replay-export replay.json
agentctl replay-load replay.json
```

The file resolves to `artifacts/replay.json` on the server. File export and load need both the relevant replay capability and `FILESYSTEM`. Without filesystem permission, use inline JSON-RPC export and load: export returns `bundle`, and load accepts exactly one of `bundle` or `path`.

## Compatibility

Snapshot, replay-log, and replay-bundle formats are version **3**. Older files are rejected; regenerate them. Format versions, Cargo versions, stable gameplay type IDs, and per-type schema versions are separate contracts.

A bundle includes its log and every referenced snapshot. The runner validates the complete bundle before activation. Checksums are consistency checks, not an authenticity signature.

The [architecture guide](../architecture.md) explains restore transactions and ownership. The [JSON-RPC reference](../reference/protocol.md) lists request and response fields.
