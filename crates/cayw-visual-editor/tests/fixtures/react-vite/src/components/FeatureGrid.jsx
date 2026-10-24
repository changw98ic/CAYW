export default function FeatureGrid() {
  return (
    <section className="features" id="work" data-testid="feature-section">
      <div className="section-heading">
        <p className="eyebrow">What we make</p>
        <h2 data-testid="feature-heading">Three static cards, one clear system.</h2>
        <p data-testid="feature-intro">
          Each card below is plain JSX in this file, not a loop. They are safe
          targets for text, style, and attribute edits.
        </p>
      </div>
      <div className="feature-grid" id="static-cards" data-testid="feature-grid">
        <article
          className="feature-card"
          id="first-card"
          data-testid="feature-card"
          data-feature-id="clarity"
        >
          <span className="feature-index" aria-hidden="true">01</span>
          <h3 data-testid="feature-title">Clarity first</h3>
          <p data-testid="feature-copy">
            Short labels, calm spacing, and a reading order that survives small
            screens.
          </p>
          <a className="text-link" href="#contact" data-testid="feature-link">
            Talk to us
          </a>
        </article>
        <article
          className="feature-card"
          id="second-card"
          data-testid="feature-card"
          data-feature-id="safety"
        >
          <span className="feature-index" aria-hidden="true">02</span>
          <h3 data-testid="feature-title">Safe changes</h3>
          <p data-testid="feature-copy">
            Static regions can be edited while loops and computed styles remain
            read-only.
          </p>
          <a className="text-link" href="#contact" data-testid="feature-link">
            Review the rules
          </a>
        </article>
        <article className="feature-card" data-testid="feature-card" data-feature-id="speed">
          <span className="feature-index" aria-hidden="true">03</span>
          <h3 data-testid="feature-title">Fast feedback</h3>
          <p data-testid="feature-copy">
            The fixture builds with Vite and runs without a backend or external
            service.
          </p>
          <a className="text-link" href="#contact" data-testid="feature-link">
            See the setup
          </a>
        </article>
      </div>
      <div id="insertion-target" className="insertion-target" aria-hidden="true"></div>
    </section>
  )
}
