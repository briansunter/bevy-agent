<script setup>
import { withBase } from 'vitepress'
</script>

<template>
  <main class="docs-home" id="main-content">
    <section class="docs-hero" aria-labelledby="hero-title">
      <div>
        <h1 id="hero-title">A game you can step, inspect, and rewind.</h1>
        <p class="intro">Give your Bevy simulation an explicit control loop. Apply an action, advance a tick, and see what changed. Save a state and try a different decision.</p>
        <div class="docs-actions">
          <a class="primary" :href="withBase('/getting-started.html')">Build your first environment</a>
          <a :href="withBase('/guides/remote-control.html')">Connect a client</a>
        </div>
        <p class="docs-meta"><span>Bevy 0.18.1</span><span>Rust 1.91+</span><span>Experimental 0.0.3</span></p>
      </div>
      <CounterDemo />
    </section>
    <section class="docs-path" aria-labelledby="path-title">
      <h2 id="path-title">Start small. Then bring your game.</h2>
      <p>One runnable counter teaches the same contract used by the full platformer.</p>
      <div class="path-grid">
        <article class="path-step"><span class="step-number">1</span><div><h3>Run a complete example</h3><p>Copy a small Rust app. Reset, step, save a snapshot, and verify a replay.</p><a :href="withBase('/getting-started.html')">Getting started</a></div></article>
        <article class="path-step"><span class="step-number">2</span><div><h3>Define your game contract</h3><p>Declare actions, expose observations, and register the state that affects gameplay.</p><a :href="withBase('/controllable-game.html')">Game integration</a></div></article>
        <article class="path-step"><span class="step-number">3</span><div><h3>Drive it from a client</h3><p>Use Rust, the CLI, Python, or JSON-RPC to inspect and control the same environment.</p><a :href="withBase('/guides/remote-control.html')">Remote control</a></div></article>
      </div>
    </section>
    <section class="docs-library" aria-labelledby="library-title">
      <h2 id="library-title">Find the next piece</h2>
      <p>The guides explain how to use the system. The reference explains its contracts.</p>
      <div class="library-links">
        <a :href="withBase('/concepts.html')"><strong>Control loop</strong><span>Actions, ticks, and observations</span></a>
        <a :href="withBase('/guides/snapshots-replay.html')"><strong>History</strong><span>Save, restore, replay, and branch</span></a>
        <a :href="withBase('/guides/python.html')"><strong>Python</strong><span>Build a small policy loop</span></a>
        <a :href="withBase('/guides/capture.html')"><strong>Rendering</strong><span>Capture software or window pixels</span></a>
        <a :href="withBase('/reference/crates.html')"><strong>Crates and API</strong><span>Choose packages and find Rust types</span></a>
        <a :href="withBase('/reference/troubleshooting.html')"><strong>Troubleshooting</strong><span>Resolve common integration errors</span></a>
      </div>
    </section>
  </main>
</template>
