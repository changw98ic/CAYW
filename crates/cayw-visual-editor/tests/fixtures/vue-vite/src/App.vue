<script setup>
import SiteHeader from './components/SiteHeader.vue'
import HeroSection from './components/HeroSection.vue'
import FeatureGrid from './components/FeatureGrid.vue'
import SecondaryNote from './components/SecondaryNote.vue'
import CounterForm from './components/CounterForm.vue'
import DynamicPanel from './components/DynamicPanel.vue'
import SiteFooter from './components/SiteFooter.vue'
</script>

<template>
  <div class="app-shell" data-testid="app-root">
    <SiteHeader />
    <main class="page-container" data-testid="page-container">
      <HeroSection />
      <FeatureGrid />
      <SecondaryNote />
      <CounterForm />
      <DynamicPanel />
    </main>
    <SiteFooter />
  </div>
</template>
