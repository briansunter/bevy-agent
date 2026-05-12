# Codex Interaction Guide

Start the sample HTTP remote:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
```

Inspect it:

```sh
cargo run -p agentctl -- info
cargo run -p agentctl -- schema
```

Reset and step:

```sh
cargo run -p agentctl -- reset --seed 42
cargo run -p agentctl -- step '{"type":"Move","x":1.0,"y":0.0}'
cargo run -p agentctl -- step-many '[{"type":"Move","x":1.0,"y":0.0},{"type":"Jump"}]'
```

Snapshot, restore, and branch:

```sh
cargo run -p agentctl -- snapshot
cargo run -p agentctl -- restore <snapshot-id>
cargo run -p agentctl -- branch --from-tick 10 --label try_jump
```

Replay files:

```sh
cargo run -p agentctl -- replay-export replay.json
cargo run -p agentctl -- replay-load replay.json
```

Security:

```sh
AGENT_TOKEN=secret cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000
cargo run -p agentctl -- --token secret info
```

For direct tool sessions where HTTP is unnecessary, use stdio:

```sh
cargo run -p sample_platformer --example remote_stdio
```

Then send one JSON-RPC request per line.
