<template>
  <section class="hero" id="top" data-testid="hero-section">
    <div class="hero-copy">
      <p class="eyebrow" data-testid="hero-eyebrow">Synthetic fixture</p>
      <h1 id="hero-title" data-testid="site-title">Build calm interfaces for busy teams.</h1>
      <p class="hero-description" id="hero-description" data-testid="hero-description">
        A small, deliberate studio site used to test visual editing across
        native headings, images, buttons, cards, and containers.
      </p>
      <div class="hero-actions">
        <button
          type="button"
          id="primary-action"
          class="button button-primary"
          data-testid="primary-cta"
        >
          Start a project
        </button>
        <a class="button button-quiet" href="#work" data-testid="secondary-cta">
          See the work
        </a>
      </div>
    </div>
    <figure class="hero-figure" data-testid="hero-figure">
      <img
        class="hero-image"
        id="original-image"
        data-testid="hero-image"
        src="/logo.svg"
        width="640"
        height="420"
        alt="Abstract Northstar logo with layered hills and a warm sun."
      />
      <figcaption class="hero-caption" data-testid="hero-caption">
        Original SVG logo created for this fixture.
      </figcaption>
    </figure>
  </section>
</template>

<style scoped>
.hero {
  position: relative;
}

.hero-image {
  background: #ffffff;
  box-shadow: 0 14px 30px rgba(19, 42, 39, 0.12);
}

.hero-caption {
  font-style: italic;
}
</style>
