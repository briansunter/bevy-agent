# Screenshots and rendering

Choose software capture for headless environments, or primary-window capture when you need the rendered Bevy window. Both are presentation of the same controlled gameplay state.

## Capture a headless game

The platformer supplies a software renderer. Start its HTTP example with an artifact root:

```sh
cargo run -p sample_platformer --example remote_http -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

Then:

```sh
agentctl reset --seed 42
agentctl step '{"type":"Move","x":1.0,"y":0.0}'
agentctl capture --out-dir screenshots --label after_step --source software
```

The PNG lands under `artifacts/screenshots/` on the server. The response includes path, tick, frame, width, and height. Inspect the image together with the symbolic observation before deciding what to do next.

## Capture a rendered window

```sh
cargo run -p sample_platformer --features visual --example remote_http_visual -- 127.0.0.1:4000 --artifact-dir ./artifacts
```

In another terminal:

```sh
agentctl capture --out-dir screenshots --label window --source primary_window
```

Enable the runner/remote `visual` feature for Bevy render/window capture support. Choose your game's own presentation plugins separately. A headless counter with no renderer does not become a visual game by enabling a feature.

## Integrate capture into your game

For headless capture, register a game-supplied renderer. For rendered capture, install `BevyRemoteControlPlugin` before `app.run()`. Primary-window screenshots and world mutation remain on the Bevy main thread.

Capture with an output directory needs both capture permission and `FILESYSTEM`. Relative paths stay under the server's configured artifact root. Library defaults deny filesystem access; the sample deliberately enables it.

Keep pixel capture, UI, and audio out of authoritative snapshots and checksums. [Game integration](../controllable-game.md#visual-capture) covers the relevant hooks.
