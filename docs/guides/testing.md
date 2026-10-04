# Testing and reproducibility

Turn the [counter tutorial](../getting-started.md) into a regression test before adding a renderer or a remote client. A useful test checks game behavior and repeatability: two equally wrong runs can still have matching checksums.

**Prerequisite:** the counter's complete `src/main.rs` and manifest, or a repository checkout. No server or GPU is needed.

## Run the packaged tests

From the repository:

```sh
cargo test -p bevy_agent_runner --example counter --locked
```

Expected: **2 passed; 0 failed**. The tests ship with the runner's counter example. If you copied the tutorial into your own project, append the following module to `src/main.rs`, then run `cargo test`:

<<< ../../crates/bevy_agent_runner/examples/counter.rs#regression

## What each test proves

| Test | Failure it can reveal |
| --- | --- |
| Reset and run the same eight actions twice | State left over from the previous episode, tick drift, inconsistent state hashes |
| Assert the counter at each tick | An incorrect increment, skipped gameplay, or an unexpected extra tick |
| Restore and repeat the next action | Missing snapshot state that affects the next transition |
| Reconstruct tick 1 and repeat | History reconstruction disagreeing with the original transition |

The counter has no random gameplay. The seed is explicit to show where a randomized game controls its RNG; this example does not test random distributions or cross-platform behavior.

## Adapt this to your game

1. Replace `build_app` with your headless app builder and `Noop` with a short legal action sequence.
2. Assert an observable game outcome: position, score, inventory, a collision, or an episode ending. Include boundary cases, not only successful movement.
3. Compare checksums at every completed tick to locate the first divergence.
4. Save just before an interesting action. Restore, repeat it, and compare both the result and hidden state that affects future play.
5. Repeat with several fixed seeds when your game uses randomness. Keep a failing seed and action sequence as a regression case.

Stop an action sequence when `done` or `truncated` is true. Test terminal behavior separately; a game need not accept another action after the episode ends.

## Diagnose a mismatch

**The game assertion fails but hashes match:** both runs are consistently wrong. Inspect the gameplay system and action semantics.

**Reset runs disagree:** look for wall-clock time, unseeded randomness, unordered systems, entity iteration order, or state that reset did not initialize.

**Reset runs agree but restore diverges:** inspect snapshot registration first. Hidden state such as a last movement direction, cooldown, RNG state, or required resource often explains the mismatch.

**Checksums match while visible behavior differs:** the checksum may omit authoritative fields, or presentation may be reading stale state. A checksum only covers what your extractor includes.

## Reproduce a remote failure

Save the environment/version, seed, action sequence, first failing tick, and returned error or observation. A [portable replay bundle](./snapshots-replay.md#export-a-portable-bundle) includes the checkpoints required to reconstruct recorded history. Keep it with the matching game schema and compatible runtime.

Test the same sequence through [HTTP and the CLI](./remote-control.md) after local simulation tests pass. Transport validation covers authentication, serialization, capabilities, and [uncertain mutation outcomes](./recovery.md); it complements the gameplay assertions above.
