<script setup>
import { computed, ref } from 'vue'

const showDetails = ref(true)
const accent = ref('#0f766e')
const accentTint = computed(() => `${accent.value}14`)
const items = [
  { id: 'one', label: 'A loop-rendered item' },
  { id: 'two', label: 'Another loop-rendered item' },
  { id: 'three', label: 'A third loop-rendered item' },
]

function toggleAccent() {
  accent.value = accent.value === '#0f766e' ? '#b45309' : '#0f766e'
}
</script>

<template>
  <section class="panel dynamic-panel" data-testid="dynamic-section">
    <p class="eyebrow">Read-only examples</p>
    <h2 data-testid="dynamic-heading">Dynamic regions stay protected.</h2>
    <p data-testid="dynamic-intro">
      The controls below change conditional content, inline styles, and a
      mapped list. These are counterexamples for the editor, not static edit
      targets.
    </p>
    <button
      type="button"
      class="button button-secondary"
      data-testid="details-toggle"
      @click="showDetails = !showDetails"
    >
      Toggle details
    </button>
    <p v-if="showDetails" class="status-banner" data-testid="status-banner">
      Details are visible. This element is conditionally rendered.
    </p>
    <p
      id="dynamic-style"
      class="dynamic-box"
      data-testid="dynamic-style-box"
      :style="{ borderColor: accent, backgroundColor: accentTint }"
    >
      This paragraph reads its border and background from component state.
    </p>
    <button
      type="button"
      class="button button-quiet"
      data-testid="accent-toggle"
      @click="toggleAccent"
    >
      Change accent
    </button>
    <ul class="dynamic-list" id="dynamic-list" data-testid="dynamic-list">
      <li
        v-for="item in items"
        :key="item.id"
        data-testid="dynamic-item"
        :data-dynamic-id="item.id"
      >
        {{ item.label }}
      </li>
    </ul>
  </section>
</template>

<style scoped>
.dynamic-panel {
  background-image: linear-gradient(135deg, #ffffff 0%, #fbfdfc 100%);
}

.status-banner {
  font-weight: 600;
}

.dynamic-box {
  box-shadow: inset 0 0 0 1px rgba(15, 118, 110, 0.08);
}

.dynamic-list li::marker {
  color: #0f766e;
}
</style>
