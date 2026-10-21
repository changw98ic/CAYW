import React, { useState, useRef, useEffect, useCallback } from 'react';

function updateSpotlightCssRule(mx, my, rx, ry) {
  if (typeof document === 'undefined') return;
  const sheets = document.styleSheets;
  if (!sheets || !sheets.length) return;
  try {
    const rules = sheets[0].cssRules;
    for (let i = rules.length - 1; i >= 0; i--) {
      const rule = rules[i];
      if (rule.selectorText === '[data-spotlight="active"]' && rule.style) {
        rule.style.setProperty('--mx', mx);
        rule.style.setProperty('--my', my);
        rule.style.setProperty('--rx', rx);
        rule.style.setProperty('--ry', ry);
        return;
      }
    }
  } catch {
    // Ignore when stylesheet rules are unavailable in headless test environments.
  }
}

export function SpotlightCard({
  as: Tag = 'article',
  tilt = false,
  border = false,
  className = '',
  handlers,
  children,
  ...rest
}) {
  const [active, setActive] = useState(false);
  const ref = useRef(null);
  const setNodeRef = node => {
    ref.current = node;
    if (node && handlers) node.__handlers = handlers;
  };

  const handlePointerMove = useCallback(
    event => {
      const node = ref.current;
      if (!node || typeof node.getBoundingClientRect !== 'function') return;
      if (!active) setActive(true);
      const rect = node.getBoundingClientRect();
      const x = event.clientX - rect.left;
      const y = event.clientY - rect.top;
      const ry =
        tilt && rect.width > 0
          ? `${(((x / rect.width) - 0.5) * 3.2).toFixed(2)}deg`
          : '0deg';
      const rx =
        tilt && rect.height > 0
          ? `${((0.5 - y / rect.height) * 2.6).toFixed(2)}deg`
          : '0deg';
      updateSpotlightCssRule(`${Math.round(x)}px`, `${Math.round(y)}px`, rx, ry);
    },
    [active, tilt],
  );

  const handlePointerLeave = useCallback(() => {
    setActive(false);
    updateSpotlightCssRule('50%', '50%', '0deg', '0deg');
  }, []);

  const classes = [
    className,
    'bits-spotlight',
    tilt ? 'bits-tilt' : '',
    border ? 'bits-border' : '',
  ]
    .filter(Boolean)
    .join(' ');

  return (
    <Tag
      ref={setNodeRef}
      className={classes}
      data-spotlight={active ? 'active' : undefined}
      onPointerMove={handlePointerMove}
      onPointerLeave={handlePointerLeave}
      {...rest}
    >
      {children}
    </Tag>
  );
}

export function ShinyText({ as: Tag = 'p', className = '', children, ...rest }) {
  const classes = ['eyebrow', 'bits-shiny', className].filter(Boolean).join(' ');
  return (
    <Tag className={classes} {...rest}>
      {children}
    </Tag>
  );
}

export function GradientText({ as: Tag = 'h1', className = '', children, ...rest }) {
  const classes = ['bits-gradient-text', className].filter(Boolean).join(' ');
  return (
    <Tag className={classes} {...rest}>
      {children}
    </Tag>
  );
}

export function HintPopover({
  label = '?',
  ariaLabel,
  chip = false,
  warn = false,
  bubbleId,
  children,
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef(null);

  useEffect(() => {
    if (!open || typeof document === 'undefined') return undefined;
    const onDocClick = event => {
      if (ref.current && typeof ref.current.contains === 'function' && !ref.current.contains(event.target)) {
        setOpen(false);
      }
    };
    const onKeyDown = event => {
      if (event.key === 'Escape') setOpen(false);
    };
    document.addEventListener('click', onDocClick);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('click', onDocClick);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [open]);

  const summaryClass = chip
    ? `hint-chip${warn ? ' warn' : ''}`
    : 'hint-trigger';

  return (
    <details
      ref={ref}
      className="hint-popover"
      open={open}
      onToggle={event => setOpen(event.currentTarget.open)}
    >
      <summary className={summaryClass} aria-label={ariaLabel}>
        {label}
      </summary>
      <div id={bubbleId} className="hint-bubble">
        {children}
      </div>
    </details>
  );
}

export function TooltipBadge({ label, tip, className = 'hint-badge' }) {
  return (
    <span className={className} tabIndex={0} data-tip={tip}>
      {label}
    </span>
  );
}
