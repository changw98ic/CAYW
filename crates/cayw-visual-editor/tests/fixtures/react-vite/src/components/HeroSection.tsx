import type {ReactElement} from 'react'

export default function HeroSection(): ReactElement {
  return (
    <section className="hero" id="top" data-testid="hero-section">
      <div className="hero-copy">
        <p className="eyebrow" data-testid="hero-eyebrow">Synthetic fixture</p>
        <h1 id="hero-title" data-testid="site-title">Build calm interfaces for busy teams.</h1>
        <p className="hero-description" id="hero-description" data-testid="hero-description">
          A small, deliberate studio site used to test visual editing across
          native headings, images, buttons, cards, and containers.
        </p>
        <div className="hero-actions">
          <button
            type="button"
            id="primary-action"
            className="button button-primary"
            data-testid="primary-cta"
          >
            Start a project
          </button>
          <a className="button button-quiet" href="#work" data-testid="secondary-cta">
            See the work
          </a>
        </div>
      </div>
      <figure className="hero-figure" data-testid="hero-figure">
        <img
          className="hero-image"
          id="original-image"
          data-testid="hero-image"
          src="/logo.svg"
          width="640"
          height="420"
          alt="Abstract Northstar logo with layered hills and a warm sun."
        />
        <figcaption className="hero-caption" data-testid="hero-caption">
          Original SVG logo created for this fixture.
        </figcaption>
      </figure>
    </section>
  )
}
