<template>
  <section class="secondary-note" data-testid="secondary-note">
    <p class="eyebrow">Separate component file</p>
    <h2 id="secondary-title" data-testid="secondary-title">
      A single imported component renders this heading.
    </h2>
    <p data-testid="secondary-copy">
      The heading lives in its own SFC, not in the page shell. It is a static
      edit target across a file boundary.
    </p>
  </section>
</template>

<style scoped>
.secondary-note {
  background: linear-gradient(135deg, #ffffff, #f2fbf8);
}

.secondary-note h2 {
  max-width: 30ch;
}
</style>
