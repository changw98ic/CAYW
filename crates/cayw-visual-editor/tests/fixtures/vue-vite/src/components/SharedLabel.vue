<script setup>
defineProps({
  text: {
    type: String,
    required: true,
  },
})
</script>

<template>
  <span class="shared-label">{{ text }}</span>
</template>

<style scoped>
.shared-label {
  border: 1px solid #cde7e0;
  background: #ecfdf8;
}
</style>
