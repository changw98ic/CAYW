<script setup>
import { ref } from 'vue'

const count = ref(0)
const name = ref('')
const submittedName = ref('')

function handleSubmit() {
  submittedName.value = name.value.trim() || 'Anonymous'
}
</script>

<template>
  <section class="panel-grid" id="contact" data-testid="interactive-section">
    <div class="panel" data-testid="counter-section">
      <p class="eyebrow">Interaction</p>
      <h2 data-testid="counter-heading">Live counter</h2>
      <p class="counter-value" data-testid="counter-value" aria-live="polite">
        {{ count }}
      </p>
      <button
        type="button"
        id="counter"
        class="button button-primary counter-button"
        data-testid="counter-button"
        @click="count += 1"
      >
        Count: {{ count }}
      </button>
      <div class="button-row">
        <button
          type="button"
          class="button button-secondary"
          data-testid="counter-decrement"
          @click="count -= 1"
        >
          Decrease
        </button>
        <button
          type="button"
          class="button button-primary"
          data-testid="counter-increment"
          @click="count += 1"
        >
          Increase
        </button>
      </div>
    </div>
    <form class="panel" data-testid="contact-form" @submit.prevent="handleSubmit">
      <p class="eyebrow">Form</p>
      <h2 data-testid="form-heading">Say hello</h2>
      <label class="field" for="name-input">
        <span>Your name</span>
        <input
          id="name-input"
          v-model="name"
          data-testid="name-input"
          placeholder="Ada Lovelace"
          autocomplete="name"
        />
      </label>
      <button type="submit" class="button button-primary" data-testid="form-submit">
        Send message
      </button>
      <p id="form-output" class="form-output" data-testid="form-output" aria-live="polite">
        {{ submittedName ? `Hello, ${submittedName}.` : 'No message yet.' }}
      </p>
    </form>
  </section>
</template>

<style scoped>
.counter-value {
  font-variant-numeric: tabular-nums;
}

.field input {
  transition: border-color 160ms ease, box-shadow 160ms ease;
}

.field input:focus {
  border-color: #0f766e;
  box-shadow: 0 0 0 3px rgba(15, 118, 110, 0.14);
}

.form-output {
  border-top: 1px solid #e2ebe7;
  padding-top: 14px;
}
</style>
