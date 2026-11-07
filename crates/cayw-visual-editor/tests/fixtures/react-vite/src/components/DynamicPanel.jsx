import { useState } from 'react'

const items = [
  { id: 'one', label: 'A loop-rendered item' },
  { id: 'two', label: 'Another loop-rendered item' },
  { id: 'three', label: 'A third loop-rendered item' },
]

export default function DynamicPanel() {
  const [showDetails, setShowDetails] = useState(true)
  const [accent, setAccent] = useState('#0f766e')

  return (
    <section className="panel dynamic-panel" data-testid="dynamic-section">
      <p className="eyebrow">Read-only examples</p>
      <h2 data-testid="dynamic-heading">Dynamic regions stay protected.</h2>
      <p data-testid="dynamic-intro">
        The controls below change conditional content, inline styles, and a
        mapped list. These are counterexamples for the editor, not static edit
        targets.
      </p>
      <button
        type="button"
        className="button button-secondary"
        data-testid="details-toggle"
        onClick={() => setShowDetails((value) => !value)}
      >
        Toggle details
      </button>
      {showDetails ? (
        <p className="status-banner" data-testid="status-banner">
          Details are visible. This element is conditionally rendered.
        </p>
      ) : null}
      <p
        id="dynamic-style"
        className="dynamic-box"
        data-testid="dynamic-style-box"
        style={{ borderColor: accent, backgroundColor: `${accent}14` }}
      >
        This paragraph reads its border and background from component state.
      </p>
      <button
        type="button"
        className="button button-quiet"
        data-testid="accent-toggle"
        onClick={() => setAccent((value) => (value === '#0f766e' ? '#b45309' : '#0f766e'))}
      >
        Change accent
      </button>
      <ul className="dynamic-list" id="dynamic-list" data-testid="dynamic-list">
        {items.map((item) => (
          <li key={item.id} data-testid="dynamic-item" data-dynamic-id={item.id}>
            {item.label}
          </li>
        ))}
      </ul>
    </section>
  )
}
