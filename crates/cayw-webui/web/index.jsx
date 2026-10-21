import React from 'react';
import { createRoot } from 'react-dom/client';
import { flushSync } from 'react-dom';
import { CAYWApp } from './App.jsx';

export * from './state.js';
export { CAYWApp };

export function mountApp(container, env) {
  const root = createRoot(container);
  flushSync(() => {
    root.render(<CAYWApp env={env} />);
  });
  return root;
}

if (typeof document !== 'undefined') {
  const container = document.getElementById('app-root');
  if (container) {
    mountApp(container);
  }
}
