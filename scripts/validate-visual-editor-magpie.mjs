// Opt-in real-Magpie generation acceptance for the React/Vue visual-editor work.
//
// Default behaviour is a safe public preflight: it prepares the two host-authored
// scaffolds, builds them, probes GET /hello and GET /v1/models without credentials,
// and never starts a model request. Generation requires all of:
//   1. MAGPIE_API_KEY in the current environment (never written to disk),
//   2. a green local React/Vue acceptance summary,
//   3. no --public-only flag.
//
// Generation uses only the public `cayw serve` IPC surface. The driver creates one
// exact approval decision per fs.write after checking the target path, the baseline
// SHA-256, the content size, and the no-instrumentation rule. It never retries a
// failed request and never switches provider or model.
//
// A generated project is not accepted by the page checks alone. After both projects
// are generated, this driver runs the existing
// scripts/validate-visual-editor-framework.mjs lifecycle driver against copies of
// the generated projects with the same editor binary. Total acceptance requires
// that lifecycle summary to pass. Host scaffolds never enter the browser acceptance
// path and are always recorded with source_origin=host_scaffold.
import {spawn, spawnSync} from 'node:child_process';
import {createHash, randomBytes} from 'node:crypto';
import {createServer, request as httpRequest} from 'node:http';
import {request as httpsRequest} from 'node:https';
import {connect} from 'node:net';
import {createRequire} from 'node:module';
import {
  access, cp, mkdir, readFile, readdir, symlink, writeFile, open
} from 'node:fs/promises';
import {constants as fsConstants} from 'node:fs';
import {dirname, resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';
import {once} from 'node:events';
import {release} from 'node:os';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..');
const MODEL = 'opencode-go/deepseek-v4.1-flash';
const TOTAL_BUDGET = 100_000_000;
const REACT_BUDGET = 45_000_000;
const VUE_BUDGET = 45_000_000;
const RESERVE_BUDGET = 10_000_000;
const WRITE_BYTES = 256 * 1024;
const TOTAL_WRITE_BYTES = 2 * 1024 * 1024;
const DEFAULT_BASE = 'http://127.0.0.1:3425/v1';
const PUBLIC_PROBE_NOTE = 'Unauthenticated GET /hello and GET /v1/models show reachability only. Their HTTP status does not prove the key exists, is authorized, has quota, or can generate.';
const SCAFFOLD_FIXTURES = {react: 'react-vite', vue: 'vue-vite'};
const ALLOWED = {
  react: ['src/App.tsx', 'src/components/SecondaryNote.tsx', 'src/components/SharedLabel.tsx', 'src/styles.css'],
  vue: ['src/App.vue', 'src/components/SecondaryNote.vue', 'src/components/SharedLabel.vue', 'src/styles.css'],
};
const SELECTORS = [
  '#hero-title', '#hero-description', '#primary-action', '#original-image',
  '#static-cards', '#first-card', '#second-card', '#insertion-target',
  '#secondary-title', '#counter', '#name-input', '#form-output', '#dynamic-list',
  '#dynamic-style',
];

if (process.platform !== 'darwin') throw new Error('This acceptance is scoped to macOS.');

function option(name, fallback = undefined) {
  const index = process.argv.indexOf(name);
  if (index < 0) return fallback;
  const value = process.argv[index + 1];
  if (!value || value.startsWith('--')) throw new Error(`${name} requires a value`);
  return value;
}
const flag = name => process.argv.includes(name);
function usage() {
  console.log(`Usage: node scripts/validate-visual-editor-magpie.mjs --out NEW_DIRECTORY [options]
  --runtime PATH            cayw binary (default .local/issue30-target/debug/cayw)
  --editor PATH             cayw-visual-editor binary; required for the full lifecycle acceptance
  --scaffold-root DIR       host fixture root (default .local/issue30-framework-inputs)
  --local-acceptance DIR    green framework summary directory (default .local/issue30-validation/framework-01)
  --ledger-helper PATH      compiled framework_magpie example
  --public-only             probes and scaffold builds only; never generate
  --no-browser              skip browser preflight and generated-page checks (not an acceptance pass)
  --help
The key is read only from MAGPIE_API_KEY in this process. No credential file is read.
Set CAYW_PLAYWRIGHT_MODULE when Playwright is not resolvable from this repository.
An unauthenticated 200 from Magpie is a reachability signal, not proof of generation permission.
All model requests, when enabled, use ${MODEL} through Magpie only.`);
}

const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const shaFile = async path => sha(await readFile(path));
const digestMap = map => sha(Object.entries(map).sort(([a], [b]) => a.localeCompare(b))
  .map(([path, digest]) => `${path}\0${digest}\n`).join(''));
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const byteLength = value => Buffer.byteLength(value, 'utf8');
function assert(value, message) { if (!value) throw new Error(message); }
function sanitizeHeaders(headers) {
  const result = {};
  for (const [key, value] of Object.entries(headers || {})) {
    const lower = key.toLowerCase();
    if (['authorization', 'proxy-authorization', 'cookie', 'set-cookie'].includes(lower)) continue;
    result[lower] = Array.isArray(value) ? value.map(String) : String(value);
  }
  return result;
}
function minimalEnv(extra = {}) {
  const env = {};
  for (const name of ['PATH', 'HOME', 'TMPDIR', 'LANG', 'LC_ALL', 'SHELL']) {
    if (process.env[name] !== undefined) env[name] = process.env[name];
  }
  env.NO_COLOR = '1';
  env.CI = '1';
  return {...env, ...extra};
}

const requireFromRepo = createRequire(resolve(root, 'package.json'));
async function loadPlaywright() {
  const candidates = [];
  if (process.env.CAYW_PLAYWRIGHT_MODULE) candidates.push(process.env.CAYW_PLAYWRIGHT_MODULE);
  candidates.push('playwright', 'playwright-core');
  const errors = [];
  for (const name of candidates) {
    try {
      const resolved = name.startsWith('/') || name.startsWith('.') ? resolve(name) : requireFromRepo.resolve(name);
      const loaded = await import(pathToFileURL(resolved).href);
      const module = loaded.chromium ? loaded : loaded.default;
      if (module?.chromium) return {module, name, resolved};
      errors.push(`${name}: module has no chromium export`);
    } catch (error) {
      errors.push(`${name}: ${String(error?.message || error)}`);
    }
  }
  return {module: null, errors};
}
async function checkLedgerHelper() {
  if (ledgerHelper) {
    try {
      await access(ledgerHelper, fsConstants.X_OK);
      const help = spawnSync(ledgerHelper, ['--help'], {encoding: 'utf8', timeout: 10000, env: minimalEnv()});
      const text = `${help.stdout || ''}${help.stderr || ''}`;
      if (help.status !== 0 || !text.includes('--snapshot')) {
        return {passed: false, mode: 'binary', path: ledgerHelper, error: `ledger helper lacks the isolated --snapshot interface: ${text.slice(-1000)}`};
      }
      return {passed: true, mode: 'binary', path: ledgerHelper, help_sha256: sha(text)};
    } catch (error) {
      return {passed: false, mode: 'binary', path: ledgerHelper, error: String(error?.message || error)};
    }
  }
  const cargo = process.env.CARGO || 'cargo';
  const result = spawnSync(cargo, ['check', '-q', '-p', 'cayw-runtime', '--example', 'framework_magpie', '--locked'], {
    cwd: root, encoding: 'utf8', timeout: 600000,
    env: minimalEnv({PATH: `${process.env.HOME}/.cargo/bin:${process.env.PATH || ''}`}),
  });
  return {
    passed: result.status === 0, mode: 'cargo', cargo,
    exit_code: result.status, signal: result.signal,
    error: result.error ? String(result.error) : undefined,
    log_tail: `${result.stdout || ''}${result.stderr || ''}`.slice(-4000),
  };
}
async function checkBrowserTooling() {
  if (noBrowser) return {passed: false, skipped: true, error: '--no-browser requested'};
  const loaded = await loadPlaywright();
  if (!loaded.module) return {passed: false, error: `Playwright is required (set CAYW_PLAYWRIGHT_MODULE): ${loaded.errors.join(' | ')}`};
  let browser;
  try {
    browser = await loaded.module.chromium.launch(process.env.CAYW_BROWSER_EXECUTABLE ? {executablePath: process.env.CAYW_BROWSER_EXECUTABLE} : {});
    return {
      passed: true, module: loaded.name, resolved: loaded.resolved,
      version: browser.version(), executable: process.env.CAYW_BROWSER_EXECUTABLE || null,
    };
  } catch (error) {
    return {passed: false, module: loaded.name, resolved: loaded.resolved, error: String(error?.message || error)};
  } finally {
    if (browser) await browser.close().catch(() => {});
  }
}

const out = resolve(option('--out') || `.local/issue30-validation/magpie-${new Date().toISOString().replace(/[-:.TZ]/g, '')}`);
const runtime = resolve(option('--runtime', '.local/issue30-target/debug/cayw'));
const editor = option('--editor') ? resolve(option('--editor')) : null;
const scaffoldRoot = resolve(option('--scaffold-root', '.local/issue30-framework-inputs'));
const localAcceptanceDir = resolve(option('--local-acceptance', '.local/issue30-validation/framework-01'));
const ledgerHelper = option('--ledger-helper') ? resolve(option('--ledger-helper')) : null;
const publicOnly = flag('--public-only');
const noBrowser = flag('--no-browser');

async function record(name, value) {
  const target = resolve(out, `${name}.json`);
  await mkdir(dirname(target), {recursive: true});
  await writeFile(target, `${JSON.stringify(value, null, 2)}\n`, {flag: 'wx'});
}
async function recordBytes(name, bytes) {
  const target = resolve(out, name);
  await mkdir(dirname(target), {recursive: true});
  await writeFile(target, bytes, {flag: 'wx'});
  await record(`${name}.meta`, {bytes: bytes.length, sha256: sha(bytes)});
}

function parseArgs() {
  if (flag('--help')) { usage(); process.exit(0); }
  assert(REACT_BUDGET + VUE_BUDGET + RESERVE_BUDGET === TOTAL_BUDGET, 'budget split must equal 100M');
  const base = new URL(process.env.MAGPIE_BASE_URL || DEFAULT_BASE);
  assert(['http:', 'https:'].includes(base.protocol), 'Magpie base URL must be HTTP(S)');
  assert(base.pathname.replace(/\/$/, '') === '/v1', 'Magpie Chat Completions base URL must end in /v1');
  assert(!base.username && !base.password && !base.search && !base.hash, 'Magpie base URL must not contain credentials, query or fragment');
  if (process.env.MAGPIE_MODEL && process.env.MAGPIE_MODEL !== MODEL) {
    throw new Error(`MAGPIE_MODEL must be exactly ${MODEL}; no model switching is allowed`);
  }
  return {base};
}

async function publicProbe(url, name) {
  const started = new Date().toISOString();
  try {
    const response = await fetch(url, {redirect: 'manual', signal: AbortSignal.timeout(6000)});
    const body = Buffer.from(await response.arrayBuffer());
    const entry = {
      url, method: 'GET', authorization_sent: false, started_utc: started,
      status: response.status, headers: sanitizeHeaders(Object.fromEntries(response.headers)),
      body_file: `${name}-body`, body_sha256: sha(body), body_bytes: body.length,
      body_utf8: body.length <= 64 * 1024 ? body.toString('utf8') : undefined,
    };
    await recordBytes(`${name}-body`, body);
    await record(name, entry);
    return entry;
  } catch (error) {
    const entry = {url, method: 'GET', authorization_sent: false, started_utc: started, error: String(error?.message || error)};
    await record(name, entry);
    return entry;
  }
}

async function checkLocalAcceptance() {
  const summaryPath = resolve(localAcceptanceDir, 'summary.json');
  const result = {directory: localAcceptanceDir, summary: summaryPath, passed: false, reasons: []};
  try {
    const summary = JSON.parse(await readFile(summaryPath, 'utf8'));
    result.summary_sha256 = await shaFile(summaryPath);
    result.real_model_requests = summary.real_model_requests;
    result.source_changed_during_run = summary.source_changed_during_run || [];
    if (summary.passed !== true) result.reasons.push('local summary.passed is not true');
    if (summary.real_model_requests !== 0) result.reasons.push('local summary did not record real_model_requests=0');
    if ((summary.source_changed_during_run || []).length) result.reasons.push('local source changed during the run');
    const cases = Array.isArray(summary.cases) ? summary.cases : [];
    for (const framework of ['react', 'vue']) {
      const edit = cases.find(item => item.name === `${framework}-edit`);
      const restart = cases.find(item => item.name === `${framework}-restart`);
      if (!edit?.passed || !restart?.passed) result.reasons.push(`${framework} local edit/restart cases are not green`);
    }
    const receiptPath = resolve(localAcceptanceDir, 'source-receipt.json');
    const receipt = JSON.parse(await readFile(receiptPath, 'utf8'));
    result.source_receipt = {path: receiptPath, files: Object.keys(receipt).length, mismatches: []};
    for (const [path, expected] of Object.entries(receipt)) {
      const actual = await shaFile(resolve(root, path)).catch(() => null);
      if (actual !== expected) result.source_receipt.mismatches.push(path);
    }
    if (result.source_receipt.mismatches.length) {
      result.reasons.push(`local acceptance source receipt differs from current files: ${result.source_receipt.mismatches.join(', ')}`);
    }
    if (editor) {
      const actual = await shaFile(editor);
      result.editor_sha256 = actual;
      if (!summary.binary_sha256) result.reasons.push('local acceptance has no binary_sha256 for --editor');
      else if (summary.binary_sha256 !== actual) {
        result.reasons.push('local acceptance binary_sha256 does not match --editor');
      }
    }
    result.passed = result.reasons.length === 0;
  } catch (error) {
    result.reasons.push(`cannot read green local acceptance: ${String(error?.message || error)}`);
  }
  return result;
}

function reactFiles() {
  return {
    'index.html': `<!doctype html>\n<html lang="en">\n  <head>\n    <meta charset="UTF-8" />\n    <meta name="viewport" content="width=device-width, initial-scale=1.0" />\n    <link rel="icon" type="image/svg+xml" href="/logo.svg" />\n    <title>Magpie React generation fixture</title>\n  </head>\n  <body>\n    <div id="root"></div>\n    <script type="module" src="/src/main.tsx"></script>\n  </body>\n</html>\n`,
    'src/main.tsx': `import { StrictMode } from 'react'\nimport { createRoot } from 'react-dom/client'\nimport App from './App'\nimport './styles.css'\n\ncreateRoot(document.getElementById('root')!).render(\n  <StrictMode>\n    <App />\n  </StrictMode>,\n)\n`,
    'src/App.tsx': `import { useState, type FormEvent } from 'react'\nimport SecondaryNote from './components/SecondaryNote'\nimport SharedLabel from './components/SharedLabel'\n\nconst dynamicItems = [\n  {id: 'one', label: 'Loop item one'},\n  {id: 'two', label: 'Loop item two'},\n  {id: 'three', label: 'Loop item three'},\n]\n\nexport default function App() {\n  const [count, setCount] = useState(0)\n  const [name, setName] = useState('')\n  const [submitted, setSubmitted] = useState('')\n  function handleSubmit(event: FormEvent<HTMLFormElement>) {\n    event.preventDefault()\n    setSubmitted(name.trim() ? \`Hello, \${name.trim()}.\` : 'Hello, visitor.')\n  }\n  return (\n    <div className="app-shell" data-testid="app-root">\n      <header className="site-header" data-testid="site-header">\n        <a className="brand" href="/" data-testid="brand-link">\n          <span className="brand-mark">N</span>Northstar Studio <SharedLabel />\n        </a>\n        <nav className="site-nav" aria-label="Primary">\n          <a href="#features">Features</a>\n          <a href="#secondary">Notes</a>\n        </nav>\n      </header>\n      <main className="page-container" data-testid="page-container">\n        <section className="hero" data-testid="hero-section">\n          <div className="hero-copy">\n            <p className="eyebrow">Synthetic generated fixture</p>\n            <h1 id="hero-title">Build calm interfaces with a clear point of view</h1>\n            <p id="hero-description" className="hero-description">\n              A deliberately small React fixture for real Magpie generation acceptance.\n            </p>\n            <div className="hero-actions">\n              <button id="primary-action" className="button button-primary" type="button">Start the project</button>\n            </div>\n          </div>\n          <figure className="hero-figure">\n            <img id="original-image" className="hero-image" src="/logo.svg" alt="Northstar synthetic logo" width="640" height="420" />\n            <figcaption className="hero-caption">Original synthetic logo supplied by the host scaffold.</figcaption>\n          </figure>\n        </section>\n        <section id="features" className="features">\n          <div className="section-heading">\n            <p className="eyebrow">Structure</p>\n            <h2>Static cards stay easy to inspect</h2>\n            <p>Every required selector is visible in the generated page and in the source files.</p>\n          </div>\n          <div id="static-cards" className="feature-grid">\n            <article id="first-card" className="feature-card">\n              <span className="feature-index">01</span>\n              <h3>Clarity first</h3>\n              <p>A calm hierarchy makes the first action obvious.</p>\n            </article>\n            <article id="second-card" className="feature-card">\n              <span className="feature-index">02</span>\n              <h3>Safe by default</h3>\n              <p>Static structure stays readable when the page grows.</p>\n            </article>\n          </div>\n          <div id="insertion-target" className="insertion-target"></div>\n        </section>\n        <SecondaryNote />\n        <section className="panel-grid">\n          <div className="panel">\n            <p className="eyebrow">Interaction</p>\n            <h2>Counter and form</h2>\n            <button id="counter" className="button button-primary counter-button" type="button" onClick={() => setCount(value => value + 1)}>\n              Count: {count}\n            </button>\n            <form className="field" onSubmit={handleSubmit}>\n              <label htmlFor="name-input">\n                Name\n                <input id="name-input" value={name} onChange={event => setName(event.target.value)} placeholder="Your name" />\n              </label>\n              <button id="form-submit" data-testid="form-submit" className="button button-primary" type="submit">Say hello</button>\n            </form>\n            <p id="form-output" className="form-output">{submitted || 'Hello, visitor.'}</p>\n          </div>\n          <div className="panel dynamic-panel">\n            <p className="eyebrow">Dynamic regions</p>\n            <h2>Loop and conditional content</h2>\n            <ul id="dynamic-list" className="dynamic-list">\n              {dynamicItems.map(item => <li key={item.id} data-dynamic-id={item.id}>{item.label}</li>)}\n            </ul>\n            <p id="dynamic-style" style={{color: count % 2 === 0 ? '#0f766e' : '#b45309'}}>\n              This color follows the counter.\n            </p>\n          </div>\n        </section>\n      </main>\n      <footer className="site-footer">\n        <div className="footer-inner"><p>Synthetic React fixture for Issue 30.</p><p>No third-party project source.</p></div>\n      </footer>\n    </div>\n  )\n}\n`,
    'src/components/SecondaryNote.tsx': `import SharedLabel from './SharedLabel'\n\nexport default function SecondaryNote() {\n  return (\n    <section id="secondary" className="secondary-note">\n      <p className="eyebrow">Secondary source file <SharedLabel /></p>\n      <h2 id="secondary-title">A second component keeps composition honest</h2>\n      <p>This heading lives in its own source file so generation must touch more than one module.</p>\n    </section>\n  )\n}\n`,
    'src/components/SharedLabel.tsx': `export default function SharedLabel() {\n  return <span className="shared-label">Shared</span>\n}\n`,
    'src/styles.css': styles(),
  };
}

function vueFiles() {
  return {
    'index.html': `<!doctype html>\n<html lang="en">\n  <head>\n    <meta charset="UTF-8" />\n    <meta name="viewport" content="width=device-width, initial-scale=1.0" />\n    <link rel="icon" type="image/svg+xml" href="/logo.svg" />\n    <title>Magpie Vue generation fixture</title>\n  </head>\n  <body>\n    <div id="app"></div>\n    <script type="module" src="/src/main.js"></script>\n  </body>\n</html>\n`,
    'src/main.js': `import { createApp } from 'vue'\nimport App from './App.vue'\nimport './styles.css'\n\ncreateApp(App).mount('#app')\n`,
    'src/App.vue': `<script setup>\nimport { ref } from 'vue'\nimport SecondaryNote from './components/SecondaryNote.vue'\nimport SharedLabel from './components/SharedLabel.vue'\n\nconst count = ref(0)\nconst name = ref('')\nconst submitted = ref('')\nfunction submit() {\n  submitted.value = name.value.trim() ? \`Hello, \${name.value.trim()}.\` : 'Hello, visitor.'\n}\nconst dynamicItems = [\n  {id: 'one', label: 'Loop item one'},\n  {id: 'two', label: 'Loop item two'},\n  {id: 'three', label: 'Loop item three'},\n]\n</script>\n\n<template>\n  <div class="app-shell" data-testid="app-root">\n    <header class="site-header" data-testid="site-header">\n      <a class="brand" href="/" data-testid="brand-link">\n        <span class="brand-mark">N</span>Northstar Studio <SharedLabel />\n      </a>\n      <nav class="site-nav" aria-label="Primary">\n        <a href="#features">Features</a>\n        <a href="#secondary">Notes</a>\n      </nav>\n    </header>\n    <main class="page-container" data-testid="page-container">\n      <section class="hero" data-testid="hero-section">\n        <div class="hero-copy">\n          <p class="eyebrow">Synthetic generated fixture</p>\n          <h1 id="hero-title">Build calm interfaces with a clear point of view</h1>\n          <p id="hero-description" class="hero-description">\n            A deliberately small Vue fixture for real Magpie generation acceptance.\n          </p>\n          <div class="hero-actions">\n            <button id="primary-action" class="button button-primary" type="button">Start the project</button>\n          </div>\n        </div>\n        <figure class="hero-figure">\n          <img id="original-image" class="hero-image" src="/logo.svg" alt="Northstar synthetic logo" width="640" height="420" />\n          <figcaption class="hero-caption">Original synthetic logo supplied by the host scaffold.</figcaption>\n        </figure>\n      </section>\n      <section id="features" class="features">\n        <div class="section-heading">\n          <p class="eyebrow">Structure</p>\n          <h2>Static cards stay easy to inspect</h2>\n          <p>Every required selector is visible in the generated page and in the source files.</p>\n        </div>\n        <div id="static-cards" class="feature-grid">\n          <article id="first-card" class="feature-card">\n            <span class="feature-index">01</span>\n            <h3>Clarity first</h3>\n            <p>A calm hierarchy makes the first action obvious.</p>\n          </article>\n          <article id="second-card" class="feature-card">\n            <span class="feature-index">02</span>\n            <h3>Safe by default</h3>\n            <p>Static structure stays readable when the page grows.</p>\n          </article>\n        </div>\n        <div id="insertion-target" class="insertion-target"></div>\n      </section>\n      <SecondaryNote />\n      <section class="panel-grid">\n        <div class="panel">\n          <p class="eyebrow">Interaction</p>\n          <h2>Counter and form</h2>\n          <button id="counter" class="button button-primary counter-button" type="button" @click="count += 1">\n            Count: {{ count }}\n          </button>\n          <form class="field" @submit.prevent="submit">\n            <label for="name-input">\n              Name\n              <input id="name-input" v-model="name" placeholder="Your name" />\n            </label>\n            <button id="form-submit" data-testid="form-submit" class="button button-primary" type="submit">Say hello</button>\n          </form>\n          <p id="form-output" class="form-output">{{ submitted || 'Hello, visitor.' }}</p>\n        </div>\n        <div class="panel dynamic-panel">\n          <p class="eyebrow">Dynamic regions</p>\n          <h2>Loop and conditional content</h2>\n          <ul id="dynamic-list" class="dynamic-list">\n            <li v-for="item in dynamicItems" :key="item.id" :data-dynamic-id="item.id">{{ item.label }}</li>\n          </ul>\n          <p id="dynamic-style" :style="{ color: count % 2 === 0 ? '#0f766e' : '#b45309' }">\n            This color follows the counter.\n          </p>\n        </div>\n      </section>\n    </main>\n    <footer class="site-footer">\n      <div class="footer-inner"><p>Synthetic Vue fixture for Issue 30.</p><p>No third-party project source.</p></div>\n    </footer>\n  </div>\n</template>\n`,
    'src/components/SecondaryNote.vue': `<script setup>\nimport SharedLabel from './SharedLabel.vue'\n</script>\n\n<template>\n  <section id="secondary" class="secondary-note">\n    <p class="eyebrow">Secondary source file <SharedLabel /></p>\n    <h2 id="secondary-title">A second component keeps composition honest</h2>\n    <p>This heading lives in its own source file so generation must touch more than one module.</p>\n  </section>\n</template>\n`,
    'src/components/SharedLabel.vue': `<template>\n  <span class="shared-label">Shared</span>\n</template>\n`,
    'src/styles.css': styles(),
  };
}

function styles() {
  return `:root {\n  color-scheme: light;\n  --ink: #132a27;\n  --muted: #526b66;\n  --paper: #f8faf7;\n  --surface: #ffffff;\n  --line: #d7e2dd;\n  --accent: #0f766e;\n  --accent-strong: #115e59;\n  --warm: #b45309;\n  font-family: "Avenir Next", Avenir, "Helvetica Neue", Helvetica, Arial, sans-serif;\n}\n* { box-sizing: border-box; }\nhtml { scroll-behavior: smooth; }\nbody { margin: 0; min-width: 320px; background: radial-gradient(circle at 12% 8%, rgba(94, 234, 212, .18), transparent 28rem), var(--paper); color: var(--ink); line-height: 1.55; }\na { color: inherit; }\nh1, h2, h3, p { overflow-wrap: anywhere; }\nbutton, input { font: inherit; }\n:focus-visible { outline: 3px solid rgba(15, 118, 110, .34); outline-offset: 3px; }\n.app-shell { min-height: 100vh; display: flex; flex-direction: column; }\n.site-header, .page-container, .footer-inner { width: min(1120px, calc(100% - 40px)); margin: 0 auto; }\n.site-header { padding: 22px 0; display: flex; align-items: center; justify-content: space-between; gap: 24px; border-bottom: 1px solid var(--line); }\n.brand { display: inline-flex; align-items: center; gap: 10px; font-weight: 700; letter-spacing: -.02em; text-decoration: none; }\n.brand-mark { display: grid; place-items: center; width: 34px; height: 34px; border-radius: 10px; background: var(--accent); color: #fff; }\n.shared-label { display: inline-flex; align-items: center; min-height: 22px; padding: 0 8px; border: 1px solid #cde7e0; border-radius: 999px; background: #ecfdf8; color: var(--accent-strong); font-size: .66rem; font-weight: 700; letter-spacing: .04em; text-transform: uppercase; white-space: nowrap; }\n.site-nav { display: flex; gap: 22px; color: var(--muted); font-size: .94rem; }\n.site-nav a { text-decoration: none; }\n.page-container { flex: 1; padding: 64px 0 88px; }\n.hero { display: grid; grid-template-columns: minmax(0, 1.05fr) minmax(280px, .95fr); gap: 52px; align-items: center; padding-bottom: 72px; }\n.eyebrow { margin: 0 0 12px; color: var(--accent); font-size: .76rem; font-weight: 700; letter-spacing: .16em; text-transform: uppercase; }\n.hero h1 { margin: 0; max-width: 13ch; font-size: clamp(2.55rem, 6vw, 4.6rem); line-height: 1.02; letter-spacing: -.055em; }\n.hero-description { max-width: 54ch; margin: 24px 0 0; color: var(--muted); font-size: 1.08rem; }\n.hero-actions { margin-top: 30px; }\n.button { display: inline-flex; align-items: center; justify-content: center; min-height: 44px; padding: 0 18px; border: 1px solid transparent; border-radius: 999px; font-weight: 700; font-size: .94rem; text-decoration: none; cursor: pointer; }\n.button-primary { background: var(--accent); color: #fff; }\n.hero-figure { margin: 0; padding: 14px; border: 1px solid var(--line); border-radius: 24px; background: rgba(255, 255, 255, .66); box-shadow: 0 18px 45px rgba(19, 42, 39, .09); }\n.hero-image { display: block; width: 100%; height: auto; border-radius: 16px; }\n.hero-caption { margin: 12px 4px 2px; color: var(--muted); font-size: .82rem; }\n.features { padding: 64px 0; border-top: 1px solid var(--line); }\n.section-heading { max-width: 640px; margin-bottom: 28px; }\n.section-heading h2, .panel h2, .secondary-note h2 { margin: 0; font-size: clamp(1.65rem, 3vw, 2.25rem); line-height: 1.15; letter-spacing: -.035em; }\n.section-heading p:not(.eyebrow), .panel > p:not(.eyebrow), .secondary-note p:not(.eyebrow) { color: var(--muted); }\n.feature-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 16px; }\n.insertion-target { min-height: 1px; }\n.feature-card { display: flex; flex-direction: column; align-items: flex-start; min-height: 220px; padding: 24px; border: 1px solid var(--line); border-radius: 20px; background: var(--surface); }\n.feature-index { color: var(--accent); font-size: .78rem; font-weight: 700; letter-spacing: .12em; }\n.feature-card h3 { margin: 36px 0 8px; font-size: 1.22rem; letter-spacing: -.025em; }\n.feature-card p { margin: 0 0 20px; color: var(--muted); font-size: .95rem; }\n.secondary-note { margin-top: 16px; padding: 30px; border: 1px solid var(--line); border-radius: 22px; background: linear-gradient(135deg, #fff, #f2fbf8); }\n.panel-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 16px; padding: 64px 0; border-top: 1px solid var(--line); }\n.panel { padding: 26px; border: 1px solid var(--line); border-radius: 22px; background: var(--surface); }\n.counter-button { min-width: 132px; margin-bottom: 14px; font-variant-numeric: tabular-nums; }\n.field { display: grid; gap: 8px; margin: 24px 0 16px; color: var(--muted); font-size: .9rem; font-weight: 700; }\n.field input { width: 100%; min-height: 46px; padding: 10px 14px; border: 1px solid var(--line); border-radius: 12px; background: var(--paper); color: var(--ink); }\n.form-output { min-height: 1.55em; margin: 18px 0 0; color: var(--accent-strong); font-weight: 700; }\n.dynamic-panel { border-style: dashed; }\n.dynamic-list { margin: 22px 0 0; padding-left: 22px; color: var(--muted); }\n.dynamic-list li + li { margin-top: 6px; }\n.site-footer { margin-top: auto; border-top: 1px solid var(--line); background: rgba(255, 255, 255, .55); }\n.footer-inner { padding: 26px 0; display: flex; flex-wrap: wrap; justify-content: space-between; gap: 10px 24px; color: var(--muted); font-size: .84rem; }\n.footer-inner p { margin: 0; }\n@media (max-width: 860px) { .hero { grid-template-columns: 1fr; gap: 34px; } .panel-grid { grid-template-columns: 1fr; } }\n@media (max-width: 560px) { .site-header, .page-container, .footer-inner { width: min(100% - 28px, 1120px); } .hero-actions .button { width: 100%; } .secondary-note, .panel, .feature-card { padding: 20px; } .feature-grid { grid-template-columns: 1fr; } }\n@media (prefers-reduced-motion: reduce) { html { scroll-behavior: auto; } }\n`;
}

async function writeTree(path, files) {
  for (const [name, content] of Object.entries(files)) {
    const target = resolve(path, name);
    await mkdir(dirname(target), {recursive: true});
    await writeFile(target, content, {flag: 'wx'});
  }
}

async function prepareProject(framework, project) {
  const fixture = resolve(scaffoldRoot, SCAFFOLD_FIXTURES[framework]);
  const projectDir = resolve(project);
  await mkdir(projectDir, {recursive: true});
  const fixtureModules = resolve(fixture, 'node_modules');
  await access(fixtureModules, fsConstants.R_OK);
  await symlink(fixtureModules, resolve(projectDir, 'node_modules'), 'dir');
  for (const name of ['package.json', 'package-lock.json', 'vite.config.js']) {
    await cp(resolve(fixture, name), resolve(projectDir, name), {errorOnExist: true, force: false});
  }
  await mkdir(resolve(projectDir, 'public'), {recursive: true});
  for (const name of ['logo.svg', 'alternate.svg']) {
    await cp(resolve(fixture, 'public', name), resolve(projectDir, 'public', name), {errorOnExist: true, force: false});
  }
  await writeTree(projectDir, framework === 'react' ? reactFiles() : vueFiles());
  return projectDir;
}

async function walkProject(project) {
  const result = {};
  async function walk(relative) {
    const directory = resolve(project, relative);
    for (const entry of await readdir(directory, {withFileTypes: true})) {
      if (entry.name.startsWith('.') || ['node_modules', 'dist', 'build'].includes(entry.name)) continue;
      const path = relative ? `${relative}/${entry.name}` : entry.name;
      if (entry.isSymbolicLink()) continue;
      if (entry.isDirectory()) await walk(path);
      else if (entry.isFile()) result[path] = await shaFile(resolve(project, path));
    }
  }
  await walk('');
  return result;
}

function buildProject(project, logName) {
  const started = new Date().toISOString();
  const result = spawnSync('npm', ['run', 'build'], {
    cwd: project, encoding: 'utf8', timeout: 180000, env: minimalEnv(),
  });
  const log = `${result.stdout || ''}${result.stderr || ''}`;
  return {started_utc: started, exit_code: result.status, signal: result.signal, log, error: result.error ? String(result.error) : undefined};
}

async function writeBuildRecord(name, build) {
  await writeFile(resolve(out, `${name}.log`), build.log, {flag: 'wx'});
  await record(name, {
    started_utc: build.started_utc, exit_code: build.exit_code, signal: build.signal,
    log_sha256: sha(build.log), log_bytes: byteLength(build.log), error: build.error,
  });
  assert(build.exit_code === 0, `${name} failed: ${build.error || build.log.slice(-2000)}`);
}

async function startRelay(base, key, directory) {
  await mkdir(directory, {recursive: true});
  const upstream = new URL(base);
  const upstreamPath = upstream.pathname.replace(/\/$/, '');
  let sequence = 0;
  const exchanges = [];
  let summaryWrite = Promise.resolve();
  const writeRecord = (name, value) => writeFile(resolve(directory, `${name}.json`),
    `${JSON.stringify(value, null, 2)}\n`, {flag: 'wx'});
  const persistSummary = () => {
    const body = `${JSON.stringify({exchanges}, null, 2)}\n`;
    summaryWrite = summaryWrite.then(() => writeFile(resolve(directory, 'summary.json'), body, {flag: 'w'}));
    return summaryWrite.catch(() => {});
  };
  const server = createServer((request, response) => {
    const number = ++sequence;
    const prefix = String(number).padStart(4, '0');
    const chunks = [];
    let requestBytes = 0;
    let rejected = false;
    request.on('data', chunk => {
      requestBytes += chunk.length;
      if (requestBytes > 16 * 1024 * 1024) {
        rejected = true;
        request.destroy(new Error('relay request body limit'));
        return;
      }
      chunks.push(chunk);
    });
    request.on('end', async () => {
      if (rejected) return;
      const body = Buffer.concat(chunks);
      const path = upstreamPath + request.url.replace(/^\/v1/, '');
      const recordBase = {
        number, method: request.method, client_path: request.url, upstream_path: path,
        request_headers: sanitizeHeaders(request.headers), request_bytes: body.length,
        request_sha256: sha(body), authorization_forwarded: true, authorization_recorded: false,
      };
      try {
        await writeFile(resolve(directory, `${prefix}.request.body`), body, {flag: 'wx'});
        await writeRecord(`${prefix}.request`, recordBase);
      } catch (error) {
        if (!response.headersSent && !response.destroyed) {
          response.writeHead(500, {'content-type': 'application/json'});
          response.end(JSON.stringify({error: {message: 'relay evidence write failed'}}));
        }
        await writeRecord(`${prefix}.relay-error`, {error: String(error?.message || error)}).catch(() => {});
        return;
      }
      const headers = {...sanitizeHeaders(request.headers)};
      delete headers['accept-encoding'];
      delete headers['connection'];
      delete headers['transfer-encoding'];
      headers['accept-encoding'] = 'identity';
      headers.authorization = `Bearer ${key}`;
      headers.host = upstream.host;
      if (body.length) headers['content-length'] = String(body.length);
      const forwarded = createServerRequest(upstream, path, request.method, headers, body);
      let finalized = false;
      const finalize = async (meta, responseBody = null) => {
        if (finalized) return;
        finalized = true;
        if (responseBody !== null) {
          await writeFile(resolve(directory, `${prefix}.response.body`), responseBody, {flag: 'wx'})
            .catch(error => { meta.response_body_error = String(error?.message || error); });
        }
        await writeRecord(`${prefix}.response`, meta)
          .catch(error => { meta.response_metadata_error = String(error?.message || error); });
        exchanges.push(meta);
        await persistSummary();
      };
      forwarded.on('response', upstreamResponse => {
        const responseChunks = [];
        let responseBytes = 0;
        let truncated = false;
        const responseHeaders = sanitizeHeaders(upstreamResponse.headers);
        delete responseHeaders['connection'];
        delete responseHeaders['transfer-encoding'];
        delete responseHeaders['keep-alive'];
        if (!response.headersSent && !response.destroyed) {
          response.writeHead(upstreamResponse.statusCode || 502, responseHeaders);
        }
        upstreamResponse.on('data', chunk => {
          responseBytes += chunk.length;
          if (responseBytes > 64 * 1024 * 1024) {
            truncated = true;
            upstreamResponse.destroy(new Error('relay response body limit'));
            if (!response.destroyed) response.destroy();
            return;
          }
          responseChunks.push(chunk);
          response.write(chunk);
        });
        upstreamResponse.on('end', async () => {
          if (!response.writableEnded) response.end();
          const responseBody = Buffer.concat(responseChunks);
          await finalize({
            ...recordBase, status: upstreamResponse.statusCode,
            response_headers: responseHeaders, response_bytes: responseBody.length,
            response_sha256: sha(responseBody), response_truncated: truncated,
            response_complete: !truncated,
          }, responseBody);
        });
        upstreamResponse.on('error', async error => {
          if (!response.destroyed) response.destroy(error);
          await finalize({...recordBase, error: String(error?.message || error), response_complete: false});
        });
      });
      forwarded.on('error', async error => {
        if (!response.headersSent && !response.destroyed) {
          response.writeHead(502, {'content-type': 'application/json'});
          response.end(JSON.stringify({error: {message: 'relay upstream failed'}}));
        }
        await finalize({...recordBase, error: String(error?.message || error), response_complete: false});
      });
      forwarded.end(body);
    });
    request.on('error', () => {});
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  return {
    url: `http://127.0.0.1:${address.port}/v1`,
    close: async () => {
      const done = new Promise(resolve => server.close(resolve));
      server.closeAllConnections?.();
      await done;
      await summaryWrite.catch(() => {});
    },
  };
}

function createServerRequest(upstream, path, method, headers, body) {
  const request = upstream.protocol === 'https:' ? httpsRequest : httpRequest;
  return request({
    hostname: upstream.hostname, port: upstream.port || (upstream.protocol === 'https:' ? 443 : 80),
    path, method, headers,
  });
}
async function prepareRuntimeConfig(framework, allocation, project, state, relayUrl) {
  await mkdir(state, {recursive: true});
  const path = resolve(state, 'config.json');
  const config = {
    model: {
      service: 'magpie', api: 'chat_completions', id: MODEL,
      base_url: relayUrl, api_version: 'v1',
      max_output_tokens: 8192, api_key_env: 'MAGPIE_API_KEY',
    },
    budgets: {
      max_total_tokens: allocation, max_turns: 12, max_run_time_ms: 300000,
      max_capability_calls: 32, max_capability_bytes: 8 * 1024 * 1024,
      host_wait_ms: 120000, host_call_timeout_ms: 120000,
    },
    permissions: [],
  };
  await writeFile(path, `${JSON.stringify(config, null, 2)}\n`, {flag: 'wx'});
  return {path, config};
}

async function waitForServer(child, name, logs) {
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    for (const line of logs.stdout.split('\n')) {
      if (!line.trim()) continue;
      try {
        const ready = JSON.parse(line);
        if (ready.listening) return ready;
      } catch {}
    }
    if (child.exitCode !== null) throw new Error(`${name} exited before readiness: ${child.exitCode}; ${logs.stderr}`);
    await sleep(25);
  }
  throw new Error(`${name} did not become ready; ${logs.stderr}`);
}

class IpcClient {
  constructor(socket) {
    this.socket = socket;
    this.buffer = '';
    this.pending = new Map();
    this.next = 0;
    this.closed = false;
    socket.setEncoding('utf8');
    socket.on('data', chunk => this.read(chunk));
    socket.on('close', () => this.fail('connection_closed', 'IPC connection closed', true));
    socket.on('error', error => this.fail('connection_error', String(error?.message || error), true));
  }
  static async connect(address, token) {
    const [host, port] = address.split(':');
    const socket = connect({host, port: Number(port)});
    await once(socket, 'connect');
    const client = new IpcClient(socket);
    const hello = await client.request('hello', {token}, 10000, 'h');
    if (hello.version !== 1) throw new Error(`unsupported IPC version ${hello.version}`);
    return client;
  }
  fail(code, message, unknown) {
    if (this.closed) return;
    this.closed = true;
    for (const {reject, timer} of this.pending.values()) {
      clearTimeout(timer);
      reject(Object.assign(new Error(message), {code, outcome_unknown: unknown}));
    }
    this.pending.clear();
  }
  read(chunk) {
    this.buffer += chunk;
    if (this.buffer.length > 64 * 1024 * 1024) {
      this.fail('frame_too_large', 'IPC frame exceeded driver limit', true);
      this.socket.destroy();
      return;
    }
    for (;;) {
      const newline = this.buffer.indexOf('\n');
      if (newline < 0) return;
      const line = this.buffer.slice(0, newline);
      this.buffer = this.buffer.slice(newline + 1);
      if (!line) continue;
      let frame;
      try { frame = JSON.parse(line); }
      catch { this.fail('protocol_error', 'invalid IPC JSON', true); this.socket.destroy(); return; }
      const entry = this.pending.get(frame.id);
      if (!entry) continue;
      this.pending.delete(frame.id);
      clearTimeout(entry.timer);
      if (frame.error) {
        const unknown = /outcome_unknown/.test(frame.error.code || '');
        entry.reject(Object.assign(new Error(frame.error.message), {code: frame.error.code, outcome_unknown: unknown}));
      } else entry.resolve(frame.result);
    }
  }
  request(method, params, timeoutMs = 30000, fixedId = null) {
    if (this.closed) return Promise.reject(Object.assign(new Error('IPC connection is closed'), {code: 'connection_closed', outcome_unknown: false}));
    const id = fixedId || `${process.pid}-${++this.next}-${randomBytes(4).toString('hex')}`;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(Object.assign(new Error(`${method} timed out; reconcile runtime state`), {code: 'request_timeout', outcome_unknown: true}));
      }, timeoutMs);
      this.pending.set(id, {resolve, reject, timer});
      this.socket.write(`${JSON.stringify({version: 1, id, method, params})}\n`, error => {
        if (error) {
          this.pending.delete(id);
          clearTimeout(timer);
          reject(Object.assign(new Error(String(error?.message || error)), {code: 'write_failed', outcome_unknown: true}));
        }
      });
    });
  }
  close() { this.closed = true; this.socket.end(); }
}

async function decideApprovals(ipc, session, framework, baseline, state) {
  const view = await ipc.request('session.get', {session_id: session}, 15000);
  const allowed = new Set(ALLOWED[framework]);
  for (const approval of view.pending_approvals || []) {
    const key = approval.id;
    if (state.handled.has(key)) continue;
    let decision = 'deny';
    let reason = 'not an approved fixture source write';
    let args;
    try { args = JSON.parse(approval.input.scope); } catch { args = null; }
    if (approval.input.capability === 'fs.read') {
      if (args && allowed.has(args.path)) {
        decision = 'allow_once'; reason = 'read of one exact allowed generated source file';
      }
    } else if (approval.input.capability === 'fs.write') {
      const path = args?.path;
      if (allowed.has(path) && args.overwrite === true && typeof args.expected_sha256 === 'string'
          && typeof args.content === 'string' && !state.writes.has(path)
          && args.expected_sha256.toLowerCase() === baseline[path]
          && !/data-cayw|__cayw_bridge/i.test(args.content)
          && byteLength(args.content) <= WRITE_BYTES
          && state.writeBytes + byteLength(args.content) <= TOTAL_WRITE_BYTES) {
        decision = 'allow_once'; reason = 'exact allowed source path, baseline CAS, and bounded content';
      } else {
        reason = 'write denied: path, CAS, size, duplicate write, or instrumentation rule';
      }
    }
    let result = null, error = null;
    try {
      result = await ipc.request('approval.decide', {
        session_id: session, approval_id: key, decision,
      }, 15000);
      state.handled.add(key);
    } catch (caught) { error = {code: caught.code, message: caught.message, outcome_unknown: caught.outcome_unknown}; }
    if (decision === 'allow_once' && !error && approval.input.capability === 'fs.write') {
      state.writes.set(args.path, sha(args.content));
      state.writeBytes += byteLength(args.content);
    }
    await state.log.write(`${JSON.stringify({
      at: new Date().toISOString(), approval_id: key, capability: approval.input.capability,
      scope: approval.input.scope, decision, reason, error, result,
    })}\n`);
    if (error?.outcome_unknown) throw Object.assign(new Error('approval decision outcome unknown'), {outcome_unknown: true});
  }
  return view;
}

function promptFor(framework, baseline, source) {
  const allowed = ALLOWED[framework];
  const sourceBlock = allowed.map(path => `--- ${path} (baseline sha256 ${baseline[path]}) ---\n${source[path]}`).join('\n\n');
  return `Generate the required source changes for the pre-created synthetic ${framework} fixture.

Rules:
- Use only the execute_code tool with Luau. Write files with cayw.call('fs.write', {path=..., content=..., overwrite=true, expected_sha256=...}).
- Write only these paths, each at most once: ${allowed.join(', ')}.
- Use the exact baseline SHA-256 shown for each file. A write to any other path, a second write to a path, a missing expected_sha256, or content over 256 KiB will be denied.
- Do not add dependencies, change package.json or vite.config.js, use a shell, use a network, or read outside the allowed source files.
- Do not add data-cayw, data-cayw-node, data-cayw-readonly, or __cayw_bridge to source.
- Preserve every required selector and interaction:
  ${SELECTORS.join(', ')}; .shared-label must appear exactly twice.
- Keep #first-card and #second-card as two literal static sibling <article> elements inside #static-cards. Do not render them from an array, map, v-for, or any loop. They must stay independently movable.
- Keep #dynamic-list as the only looped region, with at least three loop items.
- Keep a real form submission: #name-input inside a <form>, a separate submit control with data-testid="form-submit", and #form-output. Typing "Framework interaction" and submitting must make #form-output read exactly "Hello, Framework interaction.".
- Preserve #counter increment behavior and #dynamic-style dynamic color that changes when the counter changes.
- Set #hero-title text to include "${framework === 'react' ? 'Magpie React generation' : 'Magpie Vue generation'}".
- Set #secondary-title text to include "Generated secondary section".
- Keep .shared-label text containing "Shared".
- Change ${framework === 'react' ? 'src/App.tsx' : 'src/App.vue'} and the Secondary source. You may also change SharedLabel and src/styles.css. If you change CSS, put it in src/styles.css and keep it responsive.
- Make the page visually coherent at 1600 px and 390 px, with no horizontal overflow. Keep it responsive and accessible.
- Write complete file contents, not patches. Do not ask questions. When all writes are accepted, reply with one short summary.

Current source:
${sourceBlock}`;
}

async function runFramework(framework, allocation, base) {
  const project = resolve(out, 'projects', framework);
  const stateDir = resolve(out, 'state', framework);
  const key = process.env.MAGPIE_API_KEY.trim();
  await mkdir(stateDir, {recursive: true});
  const baselineBefore = await walkProject(project);
  const source = {};
  for (const path of ALLOWED[framework]) source[path] = await readFile(resolve(project, path), 'utf8');
  const relay = await startRelay(base, key, resolve(stateDir, 'relay'));
  const config = await prepareRuntimeConfig(framework, allocation, project, stateDir, relay.url);
  const token = randomBytes(32).toString('hex');
  const db = resolve(stateDir, 'sessions.db');
  const logs = {stdout: '', stderr: ''};
  const child = spawn(runtime, ['serve', '--db', db, '--workspace', project, '--listen', '127.0.0.1:0',
    '--token-env', 'CAYW_MAGPIE_IPC_TOKEN', '--config', config.path], {
    cwd: project, env: minimalEnv({MAGPIE_API_KEY: key, CAYW_MAGPIE_IPC_TOKEN: token}),
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const runtimeRun = {argv: child.spawnargs, pid: child.pid, started_utc: new Date().toISOString()};
  let ipc;
  let session;
  let log;
  let promptError = null;
  let view = null;
  try {
    child.stdout.on('data', chunk => { logs.stdout += chunk; });
    child.stderr.on('data', chunk => { logs.stderr += chunk; });
    const ready = await waitForServer(child, `${framework} runtime`, logs);
    runtimeRun.ready = ready;
    ipc = await IpcClient.connect(ready.listening, token);
    view = await ipc.request('session.create', {label: `Issue 30 Magpie ${framework} generation`}, 15000);
    session = view.id;
    const state = {handled: new Set(), writes: new Map(), writeBytes: 0};
    log = await open(resolve(stateDir, 'approvals.jsonl'), 'wx');
    state.log = log;
    const prompt = promptFor(framework, baselineBefore, source);
    await writeFile(resolve(stateDir, 'prompt.txt'), prompt, {flag: 'wx'});
    await record(`${framework}-generation-request`, {
      model: MODEL, provider: 'magpie', allocation_tokens: allocation,
      prompt_sha256: sha(prompt), prompt_bytes: byteLength(prompt),
      relay_url: relay.url, baseline_sha256: baselineBefore,
    });
    let settled = false;
    const promptResult = ipc.request('session.prompt', {session_id: session, text: prompt}, 330000);
    promptResult.then(() => { settled = true; }, () => { settled = true; });
    while (!settled) {
      await sleep(100);
      try { view = await decideApprovals(ipc, session, framework, baselineBefore, state); }
      catch (error) {
        if (settled) break;
        if (error.code === 'stale_approval') { await sleep(25); continue; }
        promptError = error;
        break;
      }
    }
    if (!promptError) {
      try { view = await promptResult; }
      catch (error) { promptError = error; }
    }
    await log.sync().catch(() => {});
    await record(`${framework}-approval-state`, {
      writes: [...state.writes.entries()].map(([path, digest]) => ({path, sha256: digest})),
      write_bytes: state.writeBytes, handled: state.handled.size,
    });
    const finalView = await ipc.request('session.get', {session_id: session}, 15000).catch(() => null);
    await record(`${framework}-session`, finalView || view);
    const events = await collectPages(ipc, 'session.events', session).catch(error => ({error: String(error?.message || error)}));
    const audit = await collectPages(ipc, 'session.audit', session).catch(error => ({error: String(error?.message || error)}));
    const grants = await ipc.request('session.grants', {session_id: session}, 15000)
      .catch(error => ({error: String(error?.message || error)}));
    await record(`${framework}-events`, events);
    await record(`${framework}-audit`, audit);
    await record(`${framework}-grants`, grants);
  } finally {
    if (log) await log.close().catch(() => {});
    if (ipc) ipc.close();
    if (child.exitCode === null) {
      const ended = once(child, 'exit');
      child.kill('SIGINT');
      const timer = setTimeout(() => { try { child.kill('SIGKILL'); } catch {} }, 15000);
      await Promise.race([ended, sleep(16000)]);
      clearTimeout(timer);
      if (child.exitCode === null) {
        try { child.kill('SIGKILL'); } catch {}
        await Promise.race([once(child, 'exit'), sleep(2000)]);
      }
    }
    await relay.close().catch(() => {});
    await writeFile(resolve(stateDir, 'runtime-stdout.log'), logs.stdout, {flag: 'wx'}).catch(() => {});
    await writeFile(resolve(stateDir, 'runtime-stderr.log'), logs.stderr, {flag: 'wx'}).catch(() => {});
    await record(`${framework}-runtime`, {
      ...runtimeRun, exit_code: child.exitCode, signal: child.signalCode,
      stdout_sha256: sha(logs.stdout), stdout_bytes: byteLength(logs.stdout),
      stderr_sha256: sha(logs.stderr), stderr_bytes: byteLength(logs.stderr),
      stdout_tail: logs.stdout.slice(-4000), stderr_tail: logs.stderr.slice(-4000),
    }).catch(() => {});
  }
  const evidence = {project, stateDir, db, config, session, baseline: baselineBefore};
  if (promptError) {
    await record(`${framework}-prompt-error`, {
      message: promptError.message, code: promptError.code,
      outcome_unknown: Boolean(promptError.outcome_unknown),
    }).catch(() => {});
    throw Object.assign(new Error(`${framework} generation failed: ${promptError.message}`), {
      cause: promptError, outcome_unknown: promptError.outcome_unknown, evidence,
    });
  }
  return evidence;
}

async function collectPages(ipc, method, session) {
  const rows = [];
  let after = 0;
  for (let page = 0; page < 1000; page += 1) {
    const result = await ipc.request(method, {session_id: session, after, limit: 1000, wait_ms: 0}, 30000);
    if (!result.length) break;
    rows.push(...result);
    const next = result[result.length - 1].cursor;
    if (!Number.isFinite(next) || next <= after) break;
    after = next;
  }
  return rows;
}

async function dumpLedger(framework, evidence) {
  const target = resolve(evidence.stateDir, 'ledger.json');
  let snapshot = resolve(evidence.stateDir, 'ledger-snapshot');
  for (let suffix = 1; await access(snapshot).then(() => true, () => false); suffix += 1) {
    snapshot = resolve(evidence.stateDir, `ledger-snapshot-${suffix}`);
  }
  const args = ['--db', evidence.db, '--workspace', evidence.project, '--config', evidence.config.path,
    '--session', evidence.session, '--snapshot', snapshot, '--out', target];
  let command, argv;
  if (ledgerHelper) {
    command = ledgerHelper; argv = args;
  } else {
    command = process.env.CARGO || 'cargo';
    argv = ['run', '-q', '-p', 'cayw-runtime', '--example', 'framework_magpie', '--locked', '--', ...args];
  }
  const result = spawnSync(command, argv, {
    cwd: root, encoding: 'utf8', timeout: 600000,
    env: minimalEnv({PATH: `${process.env.HOME}/.cargo/bin:${process.env.PATH || ''}`}),
  });
  await writeFile(resolve(evidence.stateDir, 'ledger-command.log'), `${result.stdout || ''}${result.stderr || ''}`, {flag: 'wx'});
  await record(`${framework}-ledger-command`, {
    command, argv, snapshot, exit_code: result.status, signal: result.signal,
    error: result.error ? String(result.error) : undefined,
    stdout_sha256: sha(result.stdout || ''), stderr_sha256: sha(result.stderr || ''),
  });
  assert(result.status === 0, `${framework} ledger helper failed: ${(result.stderr || '').slice(-2000)}`);
  const ledger = JSON.parse(await readFile(target, 'utf8'));
  assert(ledger.snapshot?.original_sqlite_opened === false, `${framework} ledger helper did not isolate the original database`);
  assert(ledger.snapshot?.requests_database_copied === false, `${framework} ledger helper copied the original request authority`);
  assert(Array.isArray(ledger.snapshot?.copied)
    && ledger.snapshot.copied.some(item => item.label === 'core-database' && item.verified_equal === true),
    `${framework} ledger helper did not verify the Core database copy`);
  assert(ledger.snapshot?.snapshot_dir, `${framework} ledger helper did not record its snapshot`);
  assert(ledger.model_requests && typeof ledger.model_requests === 'object', `${framework} ledger helper returned no model_requests`);
  assert(ledger.session_id === evidence.session, `${framework} ledger helper read a different session`);
  await record(`${framework}-ledger`, ledger);
  return ledger;
}

async function stopProcess(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, 'exit');
  try { child.kill('SIGINT'); } catch {}
  const timer = setTimeout(() => {
    if (child.exitCode === null && child.signalCode === null) {
      try { child.kill('SIGKILL'); } catch {}
    }
  }, 5000);
  try { await Promise.race([exited, sleep(7000)]); } finally { clearTimeout(timer); }
}

async function checkGeneratedPage(framework, project, live) {
  assert(live === true, 'refusing browser acceptance for a project that the model did not generate');
  if (noBrowser) return {skipped: true, passed: false, error: 'browser check disabled; acceptance cannot pass'};
  const loaded = await loadPlaywright();
  if (!loaded.module) return {passed: false, error: `Playwright is required (set CAYW_PLAYWRIGHT_MODULE): ${loaded.errors.join(' | ')}`};
  const playwright = loaded.module;
  const port = await new Promise((resolvePort, rejectPort) => {
    const server = createServer();
    server.once('error', rejectPort);
    server.listen(0, '127.0.0.1', () => {
      const selected = server.address().port;
      server.close(() => resolvePort(selected));
    });
  });
  const viteBin = resolve(project, 'node_modules/vite/bin/vite.js');
  const preview = spawn(process.execPath, [viteBin, 'preview', '--host', '127.0.0.1', '--port', String(port), '--strictPort'], {
    cwd: project, env: minimalEnv(), stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stdout = '', stderr = '';
  preview.stdout.on('data', chunk => { stdout += chunk; });
  preview.stderr.on('data', chunk => { stderr += chunk; });
  const url = `http://127.0.0.1:${port}/`;
  let ready = false;
  for (let attempt = 0; attempt < 120; attempt += 1) {
    try { const response = await fetch(url, {signal: AbortSignal.timeout(1000)}); if (response.ok) { ready = true; break; } } catch {}
    if (preview.exitCode !== null) break;
    await sleep(250);
  }
  if (!ready) {
    await stopProcess(preview);
    return {passed: false, url, error: `preview did not become ready: ${stderr || stdout}`};
  }
  let browser;
  const errors = [];
  const checks = [];
  const result = {url, checks, errors, passed: false};
  try {
    browser = await playwright.chromium.launch(process.env.CAYW_BROWSER_EXECUTABLE ? {executablePath: process.env.CAYW_BROWSER_EXECUTABLE} : {});
    const page = await browser.newPage({viewport: {width: 1600, height: 1100}, reducedMotion: 'reduce'});
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(url, {waitUntil: 'networkidle'});
    const counts = {};
    for (const selector of SELECTORS) counts[selector] = await page.locator(selector).count();
    for (const selector of SELECTORS) assert(counts[selector] === 1, `${selector} count is ${counts[selector]}`);
    assert(await page.locator('#static-cards #first-card').count() === 1, 'first-card not inside static-cards');
    assert(await page.locator('#static-cards #second-card').count() === 1, 'second-card not inside static-cards');
    assert(await page.locator('.shared-label').count() === 2, 'shared-label must have exactly two instances');
    assert(await page.locator('#dynamic-list li').count() >= 3, 'dynamic-list lost loop items');
    await page.waitForFunction(() => document.querySelector('#original-image')?.complete === true);
    assert(await page.locator('#original-image').evaluate(image => image.naturalWidth > 0), 'original-image did not decode');
    const title = (await page.locator('#hero-title').innerText()).toLowerCase();
    assert(title.includes(`magpie ${framework} generation`), '#hero-title lacks the generation marker');
    const secondary = (await page.locator('#secondary-title').innerText()).toLowerCase();
    assert(secondary.includes('generated secondary section'), '#secondary-title lacks the generation marker');
    assert((await page.locator('.shared-label').first().innerText()).toLowerCase().includes('shared'), 'shared-label text changed');
    assert(await page.locator('[data-cayw], [data-cayw-node], [data-cayw-readonly], [data-cayw-reason]').count() === 0, 'generated page contains editor instrumentation');
    const styleBefore = await page.locator('#dynamic-style').evaluate(element => getComputedStyle(element).color);
    assert(styleBefore !== '', 'dynamic-style has no color');
    const before = await page.locator('#counter').innerText();
    await page.locator('#counter').click();
    await page.waitForFunction(before => document.querySelector('#counter').innerText !== before, before);
    const styleAfter = await page.locator('#dynamic-style').evaluate(element => getComputedStyle(element).color);
    assert(styleAfter !== styleBefore, 'dynamic-style did not change with the counter');
    assert(await page.locator('[data-testid="form-submit"]').count() === 1, 'form-submit control is missing');
    await page.locator('#name-input').fill('Framework interaction');
    await page.locator('[data-testid="form-submit"]').click();
    await page.waitForFunction(() => document.querySelector('#form-output')?.textContent.trim() === 'Hello, Framework interaction.');
    assert((await page.locator('#form-output').innerText()).trim() === 'Hello, Framework interaction.', 'form submission did not produce the exact greeting');
    await page.setViewportSize({width: 390, height: 900});
    assert(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), 'page has horizontal overflow at 390px');
    assert(errors.length === 0, `page errors: ${errors.join('; ')}`);
    result.counts = counts;
    result.dynamic_style = {before: styleBefore, after: styleAfter};
    result.viewport_390_overflow = false;
    result.passed = true;
    checks.push('all_required_selectors', 'static_cards', 'shared_label_count', 'dynamic_list', 'image_decode',
      'generation_markers', 'no_data_cayw_in_dom', 'counter', 'form_submit', 'dynamic_style_changes', 'responsive_390', 'no_page_errors');
  } catch (error) {
    result.error = error.message;
  } finally {
    if (browser) await browser.close().catch(() => {});
    await stopProcess(preview);
  }
  return result;
}

async function compareAndValidateSources(framework, project, baseline) {
  const after = await walkProject(project);
  const changed = Object.keys(after).filter(path => after[path] !== baseline[path]);
  const allowed = new Set(ALLOWED[framework]);
  const unexpected = changed.filter(path => !allowed.has(path));
  const instrumentation = [];
  for (const path of ALLOWED[framework]) {
    const content = await readFile(resolve(project, path), 'utf8');
    if (/data-cayw|__cayw_bridge/.test(content)) instrumentation.push(path);
  }
  const entry = framework === 'react' ? 'src/App.tsx' : 'src/App.vue';
  const secondary = framework === 'react' ? 'src/components/SecondaryNote.tsx' : 'src/components/SecondaryNote.vue';
  const css = 'src/styles.css';
  const entrySource = await readFile(resolve(project, entry), 'utf8');
  const literalCards = ['first-card', 'second-card'].every(id =>
    new RegExp(`<article\\b[^>]*\\bid=["']${id}["']`).test(entrySource));
  const loopedCards = framework === 'vue' && /<article\b[^>]*v-for=/.test(entrySource);
  const formSubmit = /data-testid=["']form-submit["']/.test(entrySource);
  assert(unexpected.length === 0, `generation changed unapproved paths: ${unexpected.join(', ')}`);
  assert(changed.includes(entry), 'App source was not changed');
  assert(literalCards, 'App source lost a literal static card; cards must not come from a loop');
  assert(!loopedCards, 'App source renders the static cards with v-for');
  assert(formSubmit, 'App source lost the real form-submit control');
  assert(changed.includes(secondary), 'Secondary source was not changed');
  assert(instrumentation.length === 0, `generation source contains instrumentation: ${instrumentation.join(', ')}`);
  return {
    before: baseline, after, changed, unexpected, instrumentation,
    literal_static_cards: literalCards, form_submit: formSubmit,
    css_changed: changed.includes(css),
    shared_label_changed: changed.includes('src/components/SharedLabel.tsx') || changed.includes('src/components/SharedLabel.vue'),
  };
}

async function runFrameworkLifecycle(projects) {
  assert(editor, 'an editor binary is required for the full framework lifecycle acceptance');
  const lifecycleOut = resolve(out, 'framework-lifecycle');
  const env = minimalEnv({
    CAYW_EDITOR_BINARY: editor,
    CAYW_EDITOR_VALIDATION_OUT: lifecycleOut,
    CAYW_EDITOR_VALIDATION_REACT: projects.react,
    CAYW_EDITOR_VALIDATION_VUE: projects.vue,
    CAYW_FRAMEWORK_INPUT_KIND: 'real-model-generated-from-authored-scaffold',
  });
  if (process.env.CAYW_PLAYWRIGHT_MODULE) env.CAYW_PLAYWRIGHT_MODULE = process.env.CAYW_PLAYWRIGHT_MODULE;
  if (process.env.CAYW_BROWSER_EXECUTABLE) env.CAYW_BROWSER_EXECUTABLE = process.env.CAYW_BROWSER_EXECUTABLE;
  const runner = resolve(root, 'scripts/validate-visual-editor-framework.mjs');
  const started = new Date().toISOString();
  const run = spawnSync(process.execPath, [runner], {
    cwd: root, env, encoding: 'utf8', timeout: 30 * 60 * 1000,
  });
  const log = `${run.stdout || ''}${run.stderr || ''}`;
  await writeFile(resolve(out, 'framework-lifecycle.log'), log, {flag: 'wx'});
  const evidence = {
    runner, out: lifecycleOut, input_kind: env.CAYW_FRAMEWORK_INPUT_KIND,
    started_utc: started, exit_code: run.status, signal: run.signal,
    error: run.error ? String(run.error) : undefined, log_sha256: sha(log),
  };
  let summary = null;
  try { summary = JSON.parse(await readFile(resolve(lifecycleOut, 'summary.json'), 'utf8')); }
  catch (error) { evidence.read_error = String(error?.message || error); }
  if (summary) {
    evidence.summary_path = resolve(lifecycleOut, 'summary.json');
    evidence.summary_sha256 = await shaFile(evidence.summary_path);
    evidence.passed = summary.passed === true;
    evidence.real_model_requests = summary.real_model_requests;
    evidence.binary_sha256 = summary.binary_sha256;
    evidence.input_kind = summary.input_kind;
    evidence.source_changed_during_run = summary.source_changed_during_run || [];
  } else evidence.passed = false;
  await record('framework-lifecycle', evidence);
  try {
    assert(run.status === 0, `framework lifecycle runner exited ${run.status}${run.signal ? `/${run.signal}` : ''}: ${log.slice(-2000)}`);
    assert(summary, `framework lifecycle runner wrote no summary: ${evidence.read_error || 'missing'}`);
    assert(summary.passed === true, `framework lifecycle did not pass: ${summary.error || 'unknown'}`);
    assert(summary.real_model_requests === 0, 'framework lifecycle runner reported model requests');
    assert(summary.input_kind === 'real-model-generated-from-authored-scaffold', 'framework lifecycle input kind is wrong');
    assert(summary.binary_sha256 === await shaFile(editor), 'framework lifecycle used a different editor binary');
    assert((summary.source_changed_during_run || []).length === 0, 'product source changed during framework lifecycle');
  } catch (error) {
    throw Object.assign(error, {lifecycle: evidence});
  }
  return evidence;
}

async function main() {
  const {base} = parseArgs();
  await mkdir(out, {recursive: false});
  const started = new Date().toISOString();
  const authorization = {
    schema: 'cayw.issue30.magpie.authorization.v1',
    requested_model: MODEL, allowed_provider: 'Magpie', total_tokens: TOTAL_BUDGET,
    allocation: {react: REACT_BUDGET, vue: VUE_BUDGET, reserve: RESERVE_BUDGET},
    automatic_retries: false, automatic_model_switch: false,
    missing_usage_policy: 'stop; preserve reservation',
    credential_source: 'environment variable MAGPIE_API_KEY only',
    credential_files_read: [],
    credential_value_recorded: false,
    public_probe_allowed_without_key: ['GET /hello', 'GET /v1/models with no Authorization header'],
    public_probe_limit: PUBLIC_PROBE_NOTE,
    no_third_party_project_download: true,
  };
  await record('authorization', authorization);
  await record('invocation', {
    argv: process.argv.slice(2), cwd: process.cwd(), node: process.version,
    base_url: base.href, model: MODEL,
    runtime, editor, scaffold_root: scaffoldRoot, local_acceptance: localAcceptanceDir,
    ledger_helper: ledgerHelper, playwright_module: process.env.CAYW_PLAYWRIGHT_MODULE || null,
    cargo: process.env.CARGO || 'cargo',
  });
  await record('source-receipt', {
    script: await shaFile(fileURLToPath(import.meta.url)),
    helper: await shaFile(resolve(root, 'crates/cayw-runtime/examples/framework_magpie.rs')).catch(() => null),
    browser_runner: await shaFile(resolve(root, 'scripts/validate-visual-editor-framework-browser.js')),
    lifecycle_runner: await shaFile(resolve(root, 'scripts/validate-visual-editor-framework.mjs')),
    fixture: {
      react: {
        package: await shaFile(resolve(scaffoldRoot, 'react-vite/package.json')),
        lock: await shaFile(resolve(scaffoldRoot, 'react-vite/package-lock.json')),
      },
      vue: {
        package: await shaFile(resolve(scaffoldRoot, 'vue-vite/package.json')),
        lock: await shaFile(resolve(scaffoldRoot, 'vue-vite/package-lock.json')),
      },
    },
    runtime: await shaFile(runtime),
  });

  const ledgerPreflight = await checkLedgerHelper();
  await record('ledger-helper-preflight', ledgerPreflight);
  const browserTooling = await checkBrowserTooling();
  await record('browser-preflight', browserTooling);
  const gate = await checkLocalAcceptance();
  await record('local-acceptance', gate);
  const keyPresent = typeof process.env.MAGPIE_API_KEY === 'string' && process.env.MAGPIE_API_KEY.trim() !== '';
  const probes = {
    hello: await publicProbe(`${base.origin}/hello`, 'public/hello'),
    models: await publicProbe(`${base.origin}/v1/models`, 'public/models'),
  };
  let routePresent = false;
  try {
    const models = JSON.parse(await readFile(resolve(out, 'public/models-body'), 'utf8'));
    routePresent = Array.isArray(models.data) && models.data.some(item => item.id === MODEL);
  } catch {}
  await record('public-probe', {
    base: base.href, route: MODEL, route_present: routePresent,
    authorization_sent: false, generation_authorization_proven: false, note: PUBLIC_PROBE_NOTE,
    probes: {
      hello: {status: probes.hello.status, error: probes.hello.error, body_sha256: probes.hello.body_sha256},
      models: {status: probes.models.status, error: probes.models.error, body_sha256: probes.models.body_sha256},
    },
  });

  const summary = {
    schema: 'cayw.issue30.magpie-generation.v1', started_utc: started, platform: process.platform,
    architecture: process.arch, os_release: release(), node: process.version,
    model: MODEL, base_url: base.href, credential_present: keyPresent,
    credential_source: 'environment variable MAGPIE_API_KEY only',
    local_acceptance_passed: gate.passed,
    ledger_helper_passed: ledgerPreflight.passed,
    browser_tooling_passed: browserTooling.passed,
    public_probe_reachable: Boolean(probes.models.status || probes.models.error === undefined),
    public_probe_proves_generation_authorization: false,
    route_present: routePresent, authorization, frameworks: {}, generation_started: false,
    model_generated_content: false, acceptance_passed: false, status: 'starting',
  };
  const writeSummary = async () => {
    summary.finished_utc = new Date().toISOString();
    await record('summary', summary);
    console.log(JSON.stringify(summary, null, 2));
  };

  const projects = {};
  try {
    for (const framework of ['react', 'vue']) {
      const project = await prepareProject(framework, resolve(out, 'projects', framework));
      projects[framework] = project;
      const baseline = await walkProject(project);
      await record(`${framework}-scaffold-sources`, baseline);
      const build = buildProject(project, `${framework}-scaffold-build`);
      await writeBuildRecord(`${framework}-scaffold-build`, build);
      summary.frameworks[framework] = {
        scaffold: project, baseline_files: Object.keys(baseline).length,
        scaffold_build_exit: build.exit_code, scaffold_build_log_sha256: sha(build.log),
        source_origin: 'host_scaffold', model_generated: false,
        scaffold_digest: digestMap(baseline),
      };
    }
  } catch (error) {
    summary.status = 'scaffold_failed';
    summary.error = error.message;
    await writeSummary();
    process.exitCode = 1;
    return;
  }

  if (!keyPresent || publicOnly) {
    summary.status = 'public_preflight_complete';
    summary.generation_status = !keyPresent ? 'skipped_missing_credential' : 'skipped_public_only';
    summary.generation_started = false;
    summary.acceptance_passed = false;
    summary.projects_are_model_output = false;
    summary.generated_projects = [];
    summary.note = !keyPresent
      ? 'MAGPIE_API_KEY is absent. Only public hello/models probes and host scaffold builds ran; no project in this directory is model-generated.'
      : '--public-only requested. No model request was sent; the projects are host scaffolds, not model output.';
    await writeSummary();
    return;
  }
  if (!gate.passed) {
    summary.status = 'blocked_local_acceptance';
    summary.generation_status = 'not_started';
    summary.reasons = gate.reasons;
    await writeSummary();
    process.exitCode = 1;
    return;
  }
  if (!browserTooling.passed) {
    summary.status = 'blocked_browser_tooling';
    summary.generation_status = 'not_started';
    summary.reasons = [browserTooling.error || 'browser tooling preflight failed; set CAYW_PLAYWRIGHT_MODULE'];
    await writeSummary();
    process.exitCode = 1;
    return;
  }
  if (!ledgerPreflight.passed) {
    summary.status = 'blocked_ledger_helper';
    summary.generation_status = 'not_started';
    summary.reasons = [ledgerPreflight.error || 'framework_magpie helper preflight failed; pass --ledger-helper'];
    await writeSummary();
    process.exitCode = 1;
    return;
  }
  if (probes.models.error) {
    summary.status = 'blocked_public_unreachable';
    summary.generation_status = 'not_started';
    summary.reasons = [`public GET /v1/models failed before generation: ${probes.models.error}`];
    await writeSummary();
    process.exitCode = 1;
    return;
  }
  if (!editor) {
    summary.status = 'blocked_editor_binary';
    summary.generation_status = 'not_started';
    summary.reasons = ['--editor is required: the complete lifecycle runner must drive the same editor binary'];
    await writeSummary();
    process.exitCode = 1;
    return;
  }

  summary.generation_started = true;
  summary.status = 'generation_started';
  const results = {};
  let totalOccupied = 0;
  let currentFramework = null;
  let currentEvidence = null;
  let currentLedger = null;
  let ledgerAttempted = false;
  try {
    for (const framework of ['react', 'vue']) {
      if (framework === 'vue' && results.react?.ledger?.model_requests?.budget_unknown) {
        summary.status = 'stopped_after_unknown_budget';
        break;
      }
      const allocation = framework === 'react' ? REACT_BUDGET : VUE_BUDGET;
      currentFramework = framework;
      currentEvidence = null;
      currentLedger = null;
      ledgerAttempted = false;
      const evidence = await runFramework(framework, allocation, base);
      currentEvidence = evidence;
      ledgerAttempted = true;
      const ledger = await dumpLedger(framework, evidence);
      currentLedger = ledger;
      const settled = Number(ledger.model_requests?.settled_tokens || 0);
      const reserved = Number(ledger.model_requests?.reserved_tokens || 0);
      const occupied = settled + reserved;
      assert(Number.isSafeInteger(settled) && Number.isSafeInteger(reserved), `${framework} ledger token counts are not safe integers`);
      assert(!ledger.model_requests?.overflowed, `${framework} ledger token arithmetic overflowed`);
      assert(occupied <= allocation, `${framework} occupied ${occupied} tokens, above its ${allocation} allocation`);
      totalOccupied += occupied;
      assert(totalOccupied <= TOTAL_BUDGET, `token accounting exceeds 100M: ${totalOccupied}`);
      summary.total_occupied_tokens = totalOccupied;
      const sources = await compareAndValidateSources(framework, evidence.project, evidence.baseline);
      await record(`${framework}-source-diff`, sources);
      summary.frameworks[framework].model_generated = true;
      summary.frameworks[framework].source_origin = 'magpie_generated';
      summary.frameworks[framework].generated_digest = digestMap(sources.after);
      summary.model_generated_content = true;
      summary.projects_are_model_output = true;
      const build = buildProject(evidence.project, `${framework}-generated-build`);
      await writeBuildRecord(`${framework}-generated-build`, build);
      const browser = await checkGeneratedPage(framework, evidence.project, true);
      await record(`${framework}-browser`, browser);
      assert(browser.passed, `${framework} browser acceptance failed: ${browser.error || 'unknown'}`);
      results[framework] = {
        session: evidence.session, ledger, occupied_tokens: occupied,
        token_usage: {settled, reserved, budget_unknown: Boolean(ledger.model_requests?.budget_unknown)},
        build_exit: build.exit_code, browser: {passed: browser.passed, checks: browser.checks, error: browser.error},
        changed_files: sources.changed, css_changed: sources.css_changed,
        shared_label_changed: sources.shared_label_changed,
        literal_static_cards: sources.literal_static_cards, form_submit: sources.form_submit,
        before_digest: digestMap(evidence.baseline), after_digest: digestMap(sources.after),
      };
      summary.frameworks[framework].generation = results[framework];
      if (ledger.model_requests?.budget_unknown || Number(ledger.model_requests?.reserved_tokens || 0) > 0) {
        summary.status = 'stopped_unknown_usage_preserved';
        throw Object.assign(new Error(`${framework} usage is unknown or still reserved; stopping without retry`), {budgetUnknown: true});
      }
    }
    summary.total_occupied_tokens = totalOccupied;
    summary.lifecycle = await runFrameworkLifecycle({react: projects.react, vue: projects.vue})
      .catch(error => { throw Object.assign(error, {lifecycleFailed: true}); });
    summary.status = 'generation_passed';
    summary.acceptance_passed = true;
  } catch (error) {
    summary.status = error.lifecycleFailed ? 'framework_lifecycle_failed'
      : error.budgetUnknown ? 'stopped_unknown_usage_preserved' : 'generation_failed';
    summary.error = error.message;
    summary.acceptance_passed = false;
    if (error.lifecycle) summary.lifecycle = error.lifecycle;
    summary.total_occupied_tokens = totalOccupied;
    const evidence = currentEvidence || error.evidence;
    if (!ledgerAttempted && evidence?.session && currentFramework) {
      try {
        const ledger = await dumpLedger(currentFramework, evidence);
        const model = ledger.model_requests || {};
        summary.error_ledger = {
          status: 'ok', settled_tokens: model.settled_tokens,
          reserved_tokens: model.reserved_tokens, budget_unknown: model.budget_unknown,
        };
        totalOccupied += Number(model.settled_tokens || 0) + Number(model.reserved_tokens || 0);
        summary.total_occupied_tokens = totalOccupied;
      } catch (ledgerError) {
        summary.error_ledger = {status: 'failed', error: String(ledgerError?.message || ledgerError)};
      }
    }
    process.exitCode = 1;
  }
  await writeSummary();
}

await main();
