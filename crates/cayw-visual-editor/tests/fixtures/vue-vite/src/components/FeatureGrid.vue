<template>
  <section class="features" id="work" data-testid="feature-section">
    <div class="section-heading">
      <p class="eyebrow">What we make</p>
      <h2 data-testid="feature-heading">Three static cards, one clear system.</h2>
      <p data-testid="feature-intro">
        Each card below is plain template markup, not a loop. They are safe
        targets for text, style, and attribute edits.
      </p>
    </div>
    <div class="feature-grid" id="static-cards" data-testid="feature-grid">
      <article
        class="feature-card"
        id="first-card"
        data-testid="feature-card"
        data-feature-id="clarity"
      >
        <span class="feature-index" aria-hidden="true">01</span>
        <h3 data-testid="feature-title">Clarity first</h3>
        <p data-testid="feature-copy">
          Short labels, calm spacing, and a reading order that survives small
          screens.
        </p>
        <a class="text-link" href="#contact" data-testid="feature-link">Talk to us</a>
      </article>
      <article
        class="feature-card"
        id="second-card"
        data-testid="feature-card"
        data-feature-id="safety"
      >
        <span class="feature-index" aria-hidden="true">02</span>
        <h3 data-testid="feature-title">Safe changes</h3>
        <p data-testid="feature-copy">
          Static regions can be edited while loops and computed styles remain
          read-only.
        </p>
        <a class="text-link" href="#contact" data-testid="feature-link">Review the rules</a>
      </article>
      <article class="feature-card" data-testid="feature-card" data-feature-id="speed">
        <span class="feature-index" aria-hidden="true">03</span>
        <h3 data-testid="feature-title">Fast feedback</h3>
        <p data-testid="feature-copy">
          The fixture builds with Vite and runs without a backend or external
          service.
        </p>
        <a class="text-link" href="#contact" data-testid="feature-link">See the setup</a>
      </article>
    </div>
    <div id="insertion-target" class="insertion-target" aria-hidden="true"></div>
  </section>
</template>

<style scoped>
.feature-card {
  transition: border-color 160ms ease, transform 160ms ease;
}

.feature-card:hover {
  border-color: #9ccfc5;
  transform: translateY(-2px);
}

.feature-index {
  border-bottom: 1px solid #cde7e0;
  padding-bottom: 6px;
}
</style>
