# Actions and observations

The action catalog says what a client may ask your game to do. The observation contract says what the client can learn after a tick. Declare both before creating `AgentApp`.

## Discover before acting

```sh
agentctl action-space
agentctl observation-space
agentctl schema
```

The platformer supports movement and jumping. The counter supports only `Noop`. A successful command in one environment is not a universal game API.

## Declare supported inputs

```rust
app.set_supported_actions([AgentActionKind::Noop, AgentActionKind::Move])
    .set_supported_observation_modes([ObservationMode::Hybrid]);
```

Translate keyboard, gamepad, or network input into domain actions. Simulation systems read `CurrentInputFrame<AgentAction>` instead of consulting input devices directly.

For a custom action, declare `AgentActionKind::Custom` and register a JSON Schema through `register_custom_action_schema`. Custom input must match at least one registered schema before scheduling or replay import. Your game still defines its effects.

## Expose the full observation

The counter's extractor returns:

```rust
Observation::Domain {
    tick: world.resource::<SimClock>().tick,
    value: json!({ "count": world.resource::<Counter>().0 }),
}
```

Its JSON Schema describes the entire serialized envelope: `kind`, `tick`, and `value`. A schema for only the nested `count` object is insufficient.

An optional observation schema is compiled during registration and checked after extraction. Schema registration rejects remote references. Unsupported observation modes fail before extraction; omitted step and observe modes inherit the current validated mode.

## Include hidden state in checksums

An agent may see only partial state. Your checksum should still cover all authoritative gameplay state, including values that influence future ticks but are absent from the observation.

Use `StableHasher` and a stable field order. The [complete counter](../getting-started.md#_2-add-the-complete-environment) includes core state and the counter in its hash.

## Handle episode boundaries

After each step, check `done` and `truncated`. Batches and fast-forward stop at terminal state. When a mutation error reports `recovery_required`, successfully reset before continuing.

See [retries and recovery](./recovery.md) for errors that can occur after a mutation has begun.
