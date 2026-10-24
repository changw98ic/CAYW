export default function SecondaryNote() {
  return (
    <section className="secondary-note" data-testid="secondary-note">
      <p className="eyebrow">Separate component file</p>
      <h2 id="secondary-title" data-testid="secondary-title">
        A single imported component renders this heading.
      </h2>
      <p data-testid="secondary-copy">
        The heading lives in its own JSX file, not in the page shell. It is a
        static edit target across a file boundary.
      </p>
    </section>
  )
}
