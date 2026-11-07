import { useState } from 'react'

export default function CounterForm() {
  const [count, setCount] = useState(0)
  const [name, setName] = useState('')
  const [submittedName, setSubmittedName] = useState('')

  function handleSubmit(event) {
    event.preventDefault()
    const nextName = name.trim() || 'Anonymous'
    setSubmittedName(nextName)
  }

  return (
    <section className="panel-grid" id="contact" data-testid="interactive-section">
      <div className="panel" data-testid="counter-section">
        <p className="eyebrow">Interaction</p>
        <h2 data-testid="counter-heading">Live counter</h2>
        <p className="counter-value" data-testid="counter-value" aria-live="polite">
          {count}
        </p>
        <button
          type="button"
          id="counter"
          className="button button-primary counter-button"
          data-testid="counter-button"
          onClick={() => setCount((value) => value + 1)}
        >
          Count: {count}
        </button>
        <div className="button-row">
          <button
            type="button"
            className="button button-secondary"
            data-testid="counter-decrement"
            onClick={() => setCount((value) => value - 1)}
          >
            Decrease
          </button>
          <button
            type="button"
            className="button button-primary"
            data-testid="counter-increment"
            onClick={() => setCount((value) => value + 1)}
          >
            Increase
          </button>
        </div>
      </div>
      <form className="panel" data-testid="contact-form" onSubmit={handleSubmit}>
        <p className="eyebrow">Form</p>
        <h2 data-testid="form-heading">Say hello</h2>
        <label className="field" htmlFor="name-input">
          <span>Your name</span>
          <input
            id="name-input"
            data-testid="name-input"
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="Ada Lovelace"
            autoComplete="name"
          />
        </label>
        <button type="submit" className="button button-primary" data-testid="form-submit">
          Send message
        </button>
        <p id="form-output" className="form-output" data-testid="form-output" aria-live="polite">
          {submittedName ? `Hello, ${submittedName}.` : 'No message yet.'}
        </p>
      </form>
    </section>
  )
}
