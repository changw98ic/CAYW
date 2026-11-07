import SharedLabel from './SharedLabel.jsx'

export default function SiteHeader() {
  return (
    <header className="site-header" data-testid="site-header">
      <a className="brand" href="#top" data-testid="brand-link">
        <span className="brand-mark" aria-hidden="true">N</span>
        <span>Northstar Studio</span>
        <SharedLabel text="Independent" />
      </a>
      <nav className="site-nav" aria-label="Primary">
        <a href="#work" data-testid="nav-work">Work</a>
        <a href="#contact" data-testid="nav-contact">Contact</a>
      </nav>
    </header>
  )
}
