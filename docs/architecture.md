# Architecture

A controlled Bevy `World` is the authority. Clients, presentation, and transports adapt that world; they do not introduce another simulation model.

## Ownership

| Crate | Owns | Does not own |
| --- | --- | --- |
| Core | Schedules, clock, RNG, input validation, action arbitration, observation contracts, checksums | Snapshot storage, history navigation, sockets |
| Snapshot | Type registration, serialization, preflight, restore/remapping, retention | Replay policy and client authorization |
| Replay | Accepted input history, executed ticks, recording, branch topology | Gameplay execution and the app |
| Runner | App lifecycle, stepping, history reconstruction, coordinated checkpoints, portable bundles, capture | Serialization and lineage rules delegated to other owners |
| Remote | RPC validation, capabilities, confinement, discovery, transport, retained operation outcomes | Game rules and independent world mutation |

The runner depends on core, snapshot, and replay. Snapshot and replay are siblings. The remote bridge depends on the runner. CLI and Python communicate through the wire protocol rather than linking a second simulation.

## Tick lifecycle

`AgentApp` initializes a Bevy app, resets an episode, and runs explicit controlled ticks. `AgentDecision` runs once before a tick. `AgentPreTick`, `AgentTick`, and `AgentPostTick` complete before `AgentFinalize` extracts observations, records input, and captures periodic checkpoints.

Run authoritative gameplay in `AgentTick`. Explicitly order systems whose results depend on one another. Presentation can update independently, but must not change authoritative gameplay.

## Snapshot and restore

Every registered type has a stable wire ID and positive schema version. Gameplay entities have stable identities so restore can remap references. Required resources must be present.

Restore validates the incoming data before activating it. State coverage includes hidden values and queued input that can affect the next tick. Snapshot and replay retention are bounded; the runner coordinates checkpoints still referenced by retained history.

## Replay and branches

Replay records accepted input and executed ticks. Reconstruction starts from a compatible checkpoint, follows branch topology, and rebuilds pending input consistently. Portable bundles contain the log and all referenced snapshots, and are validated before activation.

Cargo package versions, artifact format version 3, per-type schema versions, and checksum encodings remain separate compatibility boundaries.

## Remote execution

Validate a request's shape, session token, capability, and game contract before invoking a mutation. For a rendered app, the main thread owns world mutation and primary-window screenshots. Network I/O can run separately.

HTTP and WebSocket retain uncertain operation outcomes and deduplicate a retained retry key. Stdio does not provide that ledger. An error after mutation begins reports committed state and whether reset is required.

## Verification

Unit tests exercise owner invariants. Sample integration tests exercise cross-crate transactions. The transport smoke script uses real client and server binaries. The rendered capture test checks the window boundary under Xvfb/Mesa in manual CI.

The [contributing guide](./reference/contributing.md) lists reproducible validation commands. The repository retains the [detailed architecture review](https://github.com/briansunter/bevy-agent/blob/master/docs/reference/architecture-review.md) for implementation history and deeper invariants.
