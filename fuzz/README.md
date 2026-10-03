# JSON-RPC parser fuzzing

The target exercises bounded syntax, envelope, typed-parameter, authentication,
and request-limit validation without a simulation or filesystem effects.

```sh
cargo install cargo-fuzz --locked
cargo +nightly fuzz run rpc_request -- -max_total_time=30 -max_len=1048576
```

Run these commands from `fuzz/`. The checked-in corpus includes valid methods
and malformed envelopes. CI runs the target with a bounded duration. Preserve
new minimized failures in the corpus and add a focused regression test before
fixing the parser.
