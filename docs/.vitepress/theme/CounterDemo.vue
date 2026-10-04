<script setup>
import { computed, ref } from 'vue'
const tick = ref(0)
const saved = ref(null)
const message = ref('Step twice, save, step again, then restore.')
const response = computed(() => JSON.stringify({ tick: tick.value, observation: { kind: 'Domain', value: { count: tick.value } } }, null, 2).replace(/\"value\": \{\n\s+\"count\": (\d+)\n\s+\}/, '"value": { "count": $1 }'))
function step() { tick.value++; message.value = 'One Noop action advanced exactly one tick.' }
function reset() { tick.value = 0; saved.value = null; message.value = 'Reset starts a new episode at tick zero.' }
function snapshot() { saved.value = tick.value; message.value = 'Snapshot saved. Advance a tick, then restore it.' }
function restore() { if (saved.value !== null) { tick.value = saved.value; message.value = 'Restored the saved state. Step again to repeat the result.' } }
</script>

<template>
  <section class="counter-demo" aria-label="Interactive control loop walkthrough">
    <div class="counter-heading"><span>Try the control loop</span><span>Counter</span></div>
    <div class="counter-state">
      <div><span class="counter-label">Simulation tick</span><div class="counter-value" aria-live="polite">{{ tick }}</div></div>
      <div class="counter-checkpoint"><span class="counter-label">Snapshot</span><strong>{{ saved === null ? 'Not saved' : 'Tick ' + saved }}</strong></div>
    </div>
    <div class="counter-track" aria-hidden="true"><span v-for="i in 16" :key="i" class="counter-tick" :class="{ active: i <= Math.min(tick, 16) }"></span></div>
    <div class="counter-controls"><button @click="reset">Reset</button><button class="step" @click="step">Step</button><button @click="snapshot">Save</button><button :disabled="saved === null" @click="restore">Restore</button></div>
    <pre class="counter-response">{{ response }}</pre>
    <div class="counter-hint"><p aria-live="polite">{{ message }}</p><p>A simplified walkthrough. Run the Rust example for a real Bevy environment.</p></div>
  </section>
</template>
