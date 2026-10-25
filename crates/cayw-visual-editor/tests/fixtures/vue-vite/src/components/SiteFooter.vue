<script setup>
import SharedLabel from './SharedLabel.vue'
</script>

<template>
  <footer class="site-footer" data-testid="static-footer">
    <div class="footer-inner">
      <p data-testid="footer-note">
        Northstar Studio is a synthetic fixture. No real service is contacted.
      </p>
      <p data-testid="footer-meta">
        Vue 3.5.22 · Vite 7.3.5 · MIT dependencies · <SharedLabel text="Synthetic" />
      </p>
    </div>
  </footer>
</template>

<style scoped>
.site-footer {
  background: linear-gradient(180deg, rgba(255, 255, 255, 0.35), rgba(255, 255, 255, 0.72));
}

.footer-inner {
  border-top: 1px solid transparent;
}
</style>
