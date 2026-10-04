# Troubleshooting

Start with the failed boundary: installation, environment construction, a request, gameplay state, or capture.

| Symptom | Next check |
| --- | --- |
| Cargo cannot find bevy_agent | Depend on the named runtime crates; there is no umbrella package |
| Installed CLI is unrelated | Install bevy_agent_cli; its executable is agentctl |
| Compilation requires a newer Rust | Use Rust 1.91+ and the Bevy 0.18.1 manifest in Getting started |
| AgentApp::new returns an error | Install both extractors, declare metadata/catalogs, and register required state before construction |
| An action is rejected | Query action-space and schema; match the game's supported action and custom JSON Schema |
| Observation fails validation | Schema the full envelope, including kind, tick, and value |
| Gameplay changes without a controlled step | Move authoritative systems into AgentTick; remove frame-time or presentation input from the simulation |
| Snapshot restore changes later behavior | Register hidden state, RNG, required resources, stable type identities, and gameplay entity IDs |
| Client cannot connect | Start a server; check its listener and the /rpc endpoint; the CLI alone starts no game |
| HTTP 200 contains an error | Read JSON-RPC error; verify the session token and required capability |
| Replay export or capture cannot write a file | Enable FILESYSTEM and configure an artifact root on the server |
| A PNG appears in an unexpected directory | Paths resolve on the server under the artifact root, not the client working directory |
| A timed-out step might have executed | Look up its operation or retry key before issuing another mutation |
| recovery_required is true | Successfully reset before further gameplay access |
| An old replay will not load | Current artifacts use format 3; regenerate incompatible files |
| A past tick is unavailable | Check retained checkpoints/history and memory budgets |
| Primary-window capture is unavailable | Enable visual, run a rendered app, and use the Bevy main-thread integration |

The [complete counter](../getting-started.md) is the smallest known-good integration. Compare its registrations with your app before debugging a full game.

For protocol details, use [JSON-RPC methods](./protocol.md). For design constraints, use [Architecture](../architecture.md).
