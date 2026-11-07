import SharedLabel from './SharedLabel.jsx'

export default function SiteFooter() {
  return (
    <footer className="site-footer" data-testid="static-footer">
      <div className="footer-inner">
        <p data-testid="footer-note">
          Northstar Studio is a synthetic fixture. No real service is contacted.
        </p>
        <p data-testid="footer-meta">
          React 19.1.1 · Vite 7.3.5 · MIT dependencies · <SharedLabel text="Synthetic" />
        </p>
      </div>
    </footer>
  )
}
