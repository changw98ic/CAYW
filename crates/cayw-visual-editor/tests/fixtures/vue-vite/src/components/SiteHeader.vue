<script setup>
import SharedLabel from './SharedLabel.vue'
</script>

<template>
  <header class="site-header" data-testid="site-header">
    <a class="brand" href="#top" data-testid="brand-link">
      <span class="brand-mark" aria-hidden="true">N</span>
      <span>Northstar Studio</span>
      <SharedLabel text="Independent" />
    </a>
    <nav class="site-nav" aria-label="Primary">
      <a href="#work" data-testid="nav-work">Work</a>
      <a href="#contact" data-testid="nav-contact">Contact</a>
    </nav>
  </header>
</template>

<style scoped>
.site-header {
  position: relative;
}

.brand-mark {
  background: linear-gradient(145deg, #0f766e, #115e59 58%, #134e4a);
  box-shadow: 0 8px 18px rgba(15, 118, 110, 0.24);
}

.site-nav a {
  padding-bottom: 2px;
  border-bottom: 1px solid transparent;
}

.site-nav a:hover {
  border-bottom-color: currentColor;
}
</style>
