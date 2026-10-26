#!/usr/bin/env node
/**
 * F18 / Issue #30 — external React/Vue project acceptance harness (SLICE L).
 *
 * Purpose
 * -------
 * Against a caller-supplied (or in-repo fixture) Vite React/Vue project path:
 *   1) structural Vite/framework checks
 *   2) optional npm ci/build
 *   3) real editor open → edit → Host save → reopen via cayw-visual-editor HTTP API
 *   4) optional framework source tests + cargo test -p cayw-visual-editor
 *
 * Magpie generation is NOT established by this script (darwin-scoped elsewhere).
 * Browser Playwright acceptance remains macOS-scoped
 * (validate-visual-editor-framework.mjs). This harness prefers the existing
 * visual-editor binary + /api/document|/api/operation|/api/save|/api/reload
 * surface rather than inventing Magpie E2E.
 *
 * Exit codes
 * ----------
 *   0  required checks passed
 *   1  a required check failed
 *   2  usage / configuration error
 */
import {spawn, spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {access, cp, mkdir, readFile, readdir, writeFile} from 'node:fs/promises';
import {constants as fsConstants} from 'node:fs';
import {release} from 'node:os';
import {dirname, join, resolve, relative} from 'node:path';
import {fileURLToPath} from 'node:url';
import {once} from 'node:events';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..');
const FIXTURES = {
  react: 'crates/cayw-visual-editor/tests/fixtures/react-vite',
  vue: 'crates/cayw-visual-editor/tests/fixtures/vue-vite',
};
const REQUIRED_SELECTORS = [
  '#hero-title', '#hero-description', '#primary-action', '#original-image',
  '#static-cards', '#first-card', '#second-card', '#insertion-target',
  '#secondary-title', '#counter', '#name-input', '#form-output',
  '#dynamic-list', '#dynamic-style',
];
const EDITOR_TOKEN = 'cayw-f18-external-validation-token-32chars';
const EDIT_MARKER = 'CAYW-F18-external-edit-marker';

function flag(name) { return process.argv.includes(name); }
function option(name, fallback = undefined) {
  const i = process.argv.indexOf(name);
  if (i < 0) return fallback;
  const v = process.argv[i + 1];
  if (!v || v.startsWith('--')) throw Object.assign(new Error(`${name} requires a value`), {code: 2});
  return v;
}
function usage() {
  console.log(`Usage: node scripts/validate-visual-editor-external.mjs --out NEW_DIRECTORY [options]

  --out DIR                 evidence directory (must not already exist)
  --react-project PATH      external or local React Vite project (default: in-repo fixture)
  --vue-project PATH        external or local Vue Vite project (default: in-repo fixture)
  --editor-binary PATH      cayw-visual-editor binary (default: target/debug/cayw-visual-editor)
  --skip-npm-build          skip npm ci / npm run build for each project
  --skip-editor-chain       skip open/edit/Host-save/reopen against each project
  --skip-cargo-test         skip cargo test -p cayw-visual-editor --locked
  --skip-source-test        skip node --test framework/source.test.mjs
  --probe-magpie            unauthenticated GET /hello and GET /v1/models (no key sent)
  --help

Credentials are never printed. Magpie generation is not claimed here; see
docs/validation/f18-external-project-validation.md and
scripts/validate-visual-editor-magpie.mjs (darwin).`);
}

const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');
const shaFile = async (path) => sha(await readFile(path));
function assert(cond, message, code = 1) {
  if (!cond) {
    const err = new Error(message);
    err.code = code;
    throw err;
  }
}

async function pathExists(path) {
  try { await access(path, fsConstants.F_OK); return true; } catch { return false; }
}

function run(command, args, opts = {}) {
  const started = new Date().toISOString();
  const result = spawnSync(command, args, {
    cwd: opts.cwd || root,
    env: opts.env || process.env,
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
  });
  return {
    command: [command, ...args].join(' '),
    cwd: opts.cwd || root,
    started_utc: started,
    finished_utc: new Date().toISOString(),
    status: result.status,
    signal: result.signal,
    error: result.error ? String(result.error.message || result.error) : null,
    stdout_bytes: Buffer.byteLength(result.stdout || '', 'utf8'),
    stderr_bytes: Buffer.byteLength(result.stderr || '', 'utf8'),
    stdout_tail: (result.stdout || '').slice(-4000),
    stderr_tail: (result.stderr || '').slice(-4000),
    passed: result.status === 0 && !result.error,
  };
}

async function walkFiles(dir, base = dir, out = []) {
  const entries = await readdir(dir, {withFileTypes: true});
  for (const entry of entries) {
    if (entry.name === 'node_modules' || entry.name === 'dist' || entry.name === '.git') continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) await walkFiles(full, base, out);
    else out.push(relative(base, full).split('\\').join('/'));
  }
  return out;
}

function selectorTokens(sel) {
  if (!sel.startsWith('#')) return [sel];
  const id = sel.slice(1);
  return [`id="${id}"`, `id='${id}'`, `id={\`${id}\`}`, `id={"${id}"}`, `id={'${id}'}`];
}

async function inspectProject(label, projectPath, framework) {
  const result = {
    label,
    framework,
    path: projectPath,
    source_origin: null,
    checks: [],
    passed: false,
  };
  const exists = await pathExists(projectPath);
  result.checks.push({name: 'path-exists', passed: exists});
  assert(exists, `${label}: project path missing: ${projectPath}`);

  const pkgPath = join(projectPath, 'package.json');
  const pkg = JSON.parse(await readFile(pkgPath, 'utf8'));
  result.package_name = pkg.name || null;
  result.package_private = pkg.private === true;
  result.checks.push({name: 'package-json', passed: true});

  const viteCandidates = ['vite.config.js', 'vite.config.ts', 'vite.config.mjs', 'vite.config.cjs'];
  let viteConfig = null;
  for (const name of viteCandidates) {
    if (await pathExists(join(projectPath, name))) { viteConfig = name; break; }
  }
  result.vite_config = viteConfig;
  result.checks.push({name: 'vite-config', passed: Boolean(viteConfig)});
  assert(viteConfig, `${label}: no Vite config found`);

  const page = join(projectPath, 'index.html');
  result.checks.push({name: 'index-html', passed: await pathExists(page)});
  assert(await pathExists(page), `${label}: index.html missing`);

  if (framework === 'react') {
    const hasReact = Boolean(pkg.dependencies?.react || pkg.devDependencies?.react);
    result.checks.push({name: 'react-dependency', passed: hasReact});
    assert(hasReact, `${label}: react dependency missing`);
  } else if (framework === 'vue') {
    const hasVue = Boolean(pkg.dependencies?.vue || pkg.devDependencies?.vue);
    result.checks.push({name: 'vue-dependency', passed: hasVue});
    assert(hasVue, `${label}: vue dependency missing`);
  }

  // Fixture provenance vs external: FIXTURE.md marks in-repo synthetic templates.
  const fixtureMark = await pathExists(join(projectPath, 'FIXTURE.md'));
  result.source_origin = fixtureMark ? 'in_repo_synthetic_fixture' : 'external_or_caller_supplied';
  result.fixture_mark = fixtureMark;

  const files = await walkFiles(projectPath);
  result.file_count = files.length;
  result.file_sha256 = {};
  for (const rel of files) {
    if (Object.keys(result.file_sha256).length < 200) {
      result.file_sha256[rel] = await shaFile(join(projectPath, rel));
    }
  }

  // Selector presence in source (string search; not a DOM claim).
  const sourceBlob = [];
  for (const rel of files) {
    if (!/\.(jsx?|tsx?|vue|css|html)$/.test(rel)) continue;
    sourceBlob.push(await readFile(join(projectPath, rel), 'utf8'));
  }
  const joined = sourceBlob.join('\n');
  const selectorHits = {};
  let missing = 0;
  for (const sel of REQUIRED_SELECTORS) {
    const hit = selectorTokens(sel).some((token) => joined.includes(token));
    selectorHits[sel] = hit;
    if (!hit) missing += 1;
  }
  result.selector_hits = selectorHits;
  result.selector_missing_count = missing;

  // Issue-30 fixture ids are required only for in-repo FIXTURE.md projects.
  // Caller-supplied external projects must not false-fail when those ids are absent.
  if (fixtureMark) {
    result.checks.push({
      name: 'required-selectors-in-source',
      passed: missing === 0,
      missing,
      required: true,
    });
    assert(missing === 0, `${label}: fixture missing ${missing} required selectors in source`);
  } else {
    result.checks.push({
      name: 'issue30-fixture-selectors',
      passed: true,
      required: false,
      missing,
      note: 'Informational only for caller-supplied projects. Missing Issue-30 fixture ids do not fail acceptance; structural Vite/framework checks and the editor open/edit/save/reopen chain still apply.',
    });
  }

  result.passed = result.checks.every((c) => c.passed);
  return result;
}

async function npmBuild(label, projectPath, outDir) {
  const log = {label, project: projectPath, steps: []};
  const ci = run('npm', ['ci'], {cwd: projectPath});
  await writeFile(join(outDir, `${label}-npm-ci.log`), `${ci.stdout_tail}\n---stderr---\n${ci.stderr_tail}\n`);
  log.steps.push({name: 'npm-ci', ...ci, stdout_tail: undefined, stderr_tail: undefined});
  assert(ci.passed, `${label}: npm ci failed (status=${ci.status})`);

  const build = run('npm', ['run', 'build'], {cwd: projectPath});
  await writeFile(join(outDir, `${label}-npm-build.log`), `${build.stdout_tail}\n---stderr---\n${build.stderr_tail}\n`);
  log.steps.push({name: 'npm-build', ...build, stdout_tail: undefined, stderr_tail: undefined});
  assert(build.passed, `${label}: npm run build failed (status=${build.status})`);

  const distIndex = join(projectPath, 'dist', 'index.html');
  log.dist_index_present = await pathExists(distIndex);
  assert(log.dist_index_present, `${label}: dist/index.html missing after build`);
  log.passed = true;
  return log;
}

async function publicProbe(url, name, outDir) {
  const started = new Date().toISOString();
  try {
    const response = await fetch(url, {redirect: 'manual', signal: AbortSignal.timeout(8000)});
    const body = Buffer.from(await response.arrayBuffer());
    const entry = {
      url,
      method: 'GET',
      authorization_sent: false,
      started_utc: started,
      status: response.status,
      body_bytes: body.length,
      body_sha256: sha(body),
      note: 'Unauthenticated probe only. Status does not prove key presence, quota, or generation permission.',
    };
    await writeFile(join(outDir, `${name}.json`), `${JSON.stringify(entry, null, 2)}\n`);
    return entry;
  } catch (error) {
    const entry = {
      url,
      method: 'GET',
      authorization_sent: false,
      started_utc: started,
      error: String(error?.message || error),
      note: 'Probe failed; Magpie reachability not established.',
    };
    await writeFile(join(outDir, `${name}.json`), `${JSON.stringify(entry, null, 2)}\n`);
    return entry;
  }
}

function credentialPresence() {
  return {
    MAGPIE_API_KEY: typeof process.env.MAGPIE_API_KEY === 'string' && process.env.MAGPIE_API_KEY.trim() !== '' ? 'present' : 'absent',
    MAGPIE_BASE_URL: typeof process.env.MAGPIE_BASE_URL === 'string' && process.env.MAGPIE_BASE_URL.trim() !== '' ? 'set' : 'unset',
    GATEWAY_KEY_in_process: typeof process.env.GATEWAY_KEY === 'string' && process.env.GATEWAY_KEY.trim() !== '' ? 'present' : 'absent',
    GATEWAY_URL_in_process: typeof process.env.GATEWAY_URL === 'string' && process.env.GATEWAY_URL.trim() !== '' ? 'set' : 'unset',
    note: 'Values are never written to evidence. Use scripts/load-gateway-magpie-env.sh to map /workspace/.env GATEWAY_* → MAGPIE_*.',
  };
}

async function ensureNpmInstalled(label, projectPath, outDir) {
  if (await pathExists(join(projectPath, 'node_modules'))) {
    return {skipped: true, reason: 'node_modules already present'};
  }
  const ci = run('npm', ['ci'], {cwd: projectPath});
  await writeFile(join(outDir, `${label}-editor-npm-ci.log`), `${ci.stdout_tail}\n---stderr---\n${ci.stderr_tail}\n`);
  assert(ci.passed, `${label}: npm ci (for editor chain) failed (status=${ci.status})`);
  return {skipped: false, ...ci, stdout_tail: undefined, stderr_tail: undefined};
}

function openingTag(source, start) {
  if (typeof source !== 'string' || start < 0 || start >= source.length) return '';
  if (source[start] !== '<') return '';
  const end = source.indexOf('>', start);
  if (end < 0) return '';
  return source.slice(start, end + 1);
}

function findEditableTarget(document, projectPath) {
  const snapshot = document.snapshot || document;
  const nodes = snapshot.nodes || [];
  const files = new Map((snapshot.files || []).map((f) => [f.path, f.source || '']));

  // Prefer the node whose own opening tag carries id="hero-title".
  for (const node of nodes) {
    if (!node.editable) continue;
    const source = files.get(node.file) || '';
    const open = openingTag(source, Number(node.start) || 0);
    if (open.includes('id="hero-title"') || open.includes("id='hero-title'")) {
      return {
        node,
        mode: 'text',
        marker: `${EDIT_MARKER}`,
        prefer: 'hero-title',
      };
    }
  }

  // Style edit is the most reliable Host-save exercise across React/Vue externals
  // (text edits need plain static text; many containers fail that check).
  const preferredTags = new Set(['h1', 'h2', 'p', 'div', 'section', 'span', 'button']);
  for (const node of nodes) {
    if (!node.editable || !preferredTags.has(node.tag)) continue;
    return {
      node,
      mode: 'style',
      marker: 'rgb(18, 140, 126)',
      prefer: `style-on-${node.tag}`,
    };
  }

  const any = nodes.find((n) => n.editable);
  assert(any, `${projectPath}: no editable node mapped by visual editor`);
  return {
    node: any,
    mode: 'style',
    marker: 'rgb(18, 140, 126)',
    prefer: 'first-editable-style',
  };
}

async function startEditor(binary, projectPath, stateDir, framework, name) {
  const argv = [
    '--project', projectPath,
    '--state-dir', stateDir,
    '--framework', framework,
    '--addr', '127.0.0.1:0',
    '--preview-addr', '127.0.0.1:0',
  ];
  const child = spawn(binary, argv, {
    cwd: root,
    env: {...process.env, CAYW_EDITOR_TOKEN: EDITOR_TOKEN},
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stdout = '';
  let stderr = '';
  let urls;
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  const ready = new Promise((accept, reject) => {
    child.on('error', reject);
    child.on('exit', (code, signal) => {
      if (!urls) reject(new Error(`${name} exited before ready: ${code}/${signal}; ${stderr.slice(-2000)}`));
    });
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
      if (!urls && stdout.includes('\n')) {
        try {
          urls = JSON.parse(stdout.split('\n')[0]);
          accept(urls);
        } catch (error) {
          reject(error);
        }
      }
    });
  });
  const timer = setTimeout(() => child.kill('SIGINT'), 120000);
  try {
    await ready;
  } finally {
    clearTimeout(timer);
  }
  assert(urls?.editor_url, `${name}: missing editor_url`);
  return {child, argv, name, urls, stdout: () => stdout, stderr: () => stderr, finished: false};
}

async function stopEditor(run, outDir) {
  if (run.finished) return;
  let status;
  if (run.child.exitCode === null && run.child.signalCode === null) {
    const ended = once(run.child, 'exit');
    run.child.kill('SIGINT');
    const timer = setTimeout(() => run.child.kill('SIGKILL'), 15000);
    try { status = await ended; } finally { clearTimeout(timer); }
  } else {
    status = [run.child.exitCode, run.child.signalCode];
  }
  run.finished = true;
  await writeFile(join(outDir, `${run.name}-stdout.log`), run.stdout());
  await writeFile(join(outDir, `${run.name}-stderr.log`), run.stderr());
  assert(status[0] === 0, `${run.name} did not stop cleanly: ${JSON.stringify(status)}`);
}

async function apiGet(editorUrl, path) {
  const response = await fetch(`${editorUrl.replace(/\/$/, '')}${path}`, {
    headers: {Authorization: `Bearer ${EDITOR_TOKEN}`},
    signal: AbortSignal.timeout(30000),
  });
  const body = await response.json().catch(() => null);
  return {status: response.status, body};
}

async function apiPost(editorUrl, path, payload) {
  const origin = new URL(editorUrl).origin;
  const response = await fetch(`${editorUrl.replace(/\/$/, '')}${path}`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${EDITOR_TOKEN}`,
      'Content-Type': 'application/json',
      Origin: origin,
    },
    body: JSON.stringify(payload),
    signal: AbortSignal.timeout(60000),
  });
  const body = await response.json().catch(() => null);
  return {status: response.status, body};
}

async function readAuditHostWrite(stateDir) {
  const raw = await readFile(join(stateDir, 'calls.jsonl'), 'utf8').catch(() => '');
  const rows = raw.trim() ? raw.trim().split('\n').map((line) => JSON.parse(line)) : [];
  const writes = rows.filter((row) => row.capability === 'fs.write');
  return {
    rows: rows.length,
    fs_write_events: writes.length,
    has_fs_write: writes.length > 0,
  };
}

async function digestTree(projectPath) {
  const files = await walkFiles(projectPath);
  const digests = {};
  for (const rel of files) {
    digests[rel] = await shaFile(join(projectPath, rel));
  }
  return digests;
}

async function exerciseEditorChain(label, projectPath, framework, binary, outDir) {
  const result = {
    label,
    framework,
    project: projectPath,
    binary,
    steps: [],
    passed: false,
    claims: {
      magpie_generation: false,
      browser_playwright: false,
      editor_http_open_edit_host_save_reopen: true,
    },
  };

  assert(await pathExists(binary), `${label}: editor binary missing: ${binary}`, 2);
  const npm = await ensureNpmInstalled(label, projectPath, outDir);
  result.steps.push({name: 'ensure-npm', ...npm});

  const stateA = join(outDir, `${label}-editor-state-open`);
  const stateB = join(outDir, `${label}-editor-state-reopen`);
  await mkdir(stateA, {recursive: true});
  await mkdir(stateB, {recursive: true});

  const before = await digestTree(projectPath);
  let run = await startEditor(binary, projectPath, stateA, framework, `${label}-editor-open`);
  result.steps.push({name: 'open', urls: run.urls, argv: run.argv});

  try {
    const doc = await apiGet(run.urls.editor_url, '/api/document');
    assert(doc.status === 200, `${label}: /api/document failed (${doc.status})`);
    assert(doc.body?.snapshot?.adapter === framework, `${label}: adapter mismatch`);
    result.steps.push({
      name: 'document-open',
      status: doc.status,
      adapter: doc.body.snapshot.adapter,
      files: (doc.body.snapshot.files || []).map((f) => f.path),
      node_count: (doc.body.snapshot.nodes || []).length,
      baseline_sha256: doc.body.snapshot.sha256,
    });

    const target = findEditableTarget(doc.body, projectPath);
    const baseline = doc.body.snapshot.sha256;
    assert(baseline && /^[0-9a-f]{64}$/i.test(baseline), `${label}: missing baseline sha256`);

    let operation;
    if (target.mode === 'text') {
      operation = {kind: 'text', id: target.node.id, text: target.marker};
    } else {
      operation = {kind: 'style', id: target.node.id, properties: {color: target.marker}};
    }
    const edited = await apiPost(run.urls.editor_url, '/api/operation', operation);
    assert(edited.status === 200, `${label}: /api/operation failed (${edited.status}): ${JSON.stringify(edited.body)?.slice(0, 500)}`);
    assert(edited.body?.dirty === true, `${label}: edit did not mark draft dirty`);
    const afterEditDisk = await digestTree(projectPath);
    assert(
      JSON.stringify(afterEditDisk) === JSON.stringify(before),
      `${label}: unsaved draft changed project files on disk`,
    );
    result.steps.push({
      name: 'edit',
      status: edited.status,
      mode: target.mode,
      prefer: target.prefer,
      file: target.node.file,
      tag: target.node.tag,
      dirty: edited.body.dirty,
      disk_unchanged_until_save: true,
    });

    const saved = await apiPost(run.urls.editor_url, '/api/save', {expected_sha256: baseline});
    assert(saved.status === 200, `${label}: /api/save failed (${saved.status}): ${JSON.stringify(saved.body)?.slice(0, 500)}`);
    assert(saved.body?.dirty === false, `${label}: save left draft dirty`);
    const afterSave = await digestTree(projectPath);
    const changed = Object.keys(afterSave).filter((path) => afterSave[path] !== before[path]);
    assert(changed.length >= 1, `${label}: save wrote no source change`);
    const savedSource = await readFile(join(projectPath, target.node.file), 'utf8');
    const markerPresent =
      savedSource.includes(target.marker) ||
      savedSource.includes(EDIT_MARKER) ||
      savedSource.includes('rgb(18, 140, 126)') ||
      savedSource.includes('18, 140, 126') ||
      savedSource.includes("'18, 140, 126'") ||
      /color\s*:\s*['"]?rgb\(18,\s*140,\s*126\)/.test(savedSource);
    assert(markerPresent, `${label}: saved file ${target.node.file} does not contain edit marker`);
    assert(!savedSource.includes('data-cayw-node'), `${label}: instrumentation leaked into saved source`);

    const auditA = await readAuditHostWrite(stateA);
    assert(auditA.has_fs_write, `${label}: no Host fs.write recorded in calls.jsonl`);
    result.steps.push({
      name: 'save',
      status: saved.status,
      changed_files: changed,
      host_audit: auditA,
      new_baseline_sha256: saved.body.snapshot?.sha256 || null,
    });
  } finally {
    await stopEditor(run, outDir);
  }

  // Reopen: fresh editor process + state against the same project path.
  run = await startEditor(binary, projectPath, stateB, framework, `${label}-editor-reopen`);
  result.steps.push({name: 'reopen', urls: run.urls, argv: run.argv});
  try {
    const again = await apiGet(run.urls.editor_url, '/api/document');
    assert(again.status === 200, `${label}: reopen /api/document failed (${again.status})`);
    const files = again.body?.snapshot?.files || [];
    const fileEntry = files.find((f) => f.path === result.steps.find((s) => s.name === 'edit')?.file);
    const sourceBlob = files.map((f) => f.source || '').join('\n');
    assert(
      sourceBlob.includes(EDIT_MARKER) ||
        sourceBlob.includes('rgb(18, 140, 126)') ||
        sourceBlob.includes('18, 140, 126') ||
        (fileEntry && String(fileEntry.source || '').includes(EDIT_MARKER)),
      `${label}: reopen document lost the saved edit`,
    );
    assert(again.body?.dirty === false, `${label}: reopen unexpectedly dirty`);
    result.steps.push({
      name: 'document-reopen',
      status: again.status,
      dirty: again.body.dirty,
      retained_edit: true,
      adapter: again.body.snapshot.adapter,
    });
  } finally {
    await stopEditor(run, outDir);
  }

  result.passed = true;
  await writeFile(join(outDir, `${label}-editor-chain.json`), `${JSON.stringify(result, null, 2)}\n`);
  return result;
}

async function main() {
  if (flag('--help')) { usage(); process.exit(0); }
  const out = resolve(option('--out') || '');
  if (!out) { usage(); process.exit(2); }
  if (await pathExists(out)) {
    throw Object.assign(new Error(`--out must be a new directory: ${out}`), {code: 2});
  }
  await mkdir(dirname(out), {recursive: true});
  await mkdir(out, {recursive: false});

  const skipNpm = flag('--skip-npm-build');
  const skipCargo = flag('--skip-cargo-test');
  const skipSource = flag('--skip-source-test');
  const skipEditor = flag('--skip-editor-chain');
  const probe = flag('--probe-magpie');
  const editorBinary = resolve(
    option('--editor-binary') || join(root, 'target/debug/cayw-visual-editor'),
  );

  const work = join(out, 'work');
  await mkdir(work, {recursive: true});

  const reactSrc = option('--react-project')
    ? resolve(option('--react-project'))
    : resolve(root, FIXTURES.react);
  const vueSrc = option('--vue-project')
    ? resolve(option('--vue-project'))
    : resolve(root, FIXTURES.vue);

  // Copy into work/ so npm installs and editor Host writes never dirty caller trees.
  const reactWork = join(work, 'react-project');
  const vueWork = join(work, 'vue-project');
  await cp(reactSrc, reactWork, {recursive: true, filter: (src) => {
    const base = src.split(/[/\\]/).pop();
    return base !== 'node_modules' && base !== 'dist';
  }});
  await cp(vueSrc, vueWork, {recursive: true, filter: (src) => {
    const base = src.split(/[/\\]/).pop();
    return base !== 'node_modules' && base !== 'dist';
  }});

  const summary = {
    schema: 'cayw.f18.external-project-validation.v2',
    slice: 'L',
    feature: 'F18',
    issue: 30,
    started_utc: new Date().toISOString(),
    platform: process.platform,
    architecture: process.arch,
    os_release: release(),
    node: process.version,
    base_commit: run('git', ['rev-parse', 'HEAD']).stdout_tail.trim(),
    out,
    credentials: credentialPresence(),
    magpie_generation: {
      status: process.platform === 'darwin' ? 'not_run_by_this_harness' : 'darwin_blocked',
      established_by_this_script: false,
      macos_driver: 'scripts/validate-visual-editor-magpie.mjs',
      credential_loader: 'scripts/load-gateway-magpie-env.sh',
      note: 'This harness does not establish Magpie acceptance. Real Magpie generation remains macOS-scoped.',
    },
    projects: {},
    steps: {},
    passed: false,
  };

  summary.projects.react = await inspectProject('react', reactWork, 'react');
  summary.projects.react.source_path = reactSrc;
  summary.projects.vue = await inspectProject('vue', vueWork, 'vue');
  summary.projects.vue.source_path = vueSrc;

  if (!skipNpm) {
    summary.steps.react_npm = await npmBuild('react', reactWork, out);
    summary.steps.vue_npm = await npmBuild('vue', vueWork, out);
  } else {
    summary.steps.npm = {skipped: true};
  }

  if (!skipEditor) {
    summary.steps.react_editor_chain = await exerciseEditorChain(
      'react', reactWork, 'react', editorBinary, out,
    );
    summary.steps.vue_editor_chain = await exerciseEditorChain(
      'vue', vueWork, 'vue', editorBinary, out,
    );
  } else {
    summary.steps.editor_chain = {skipped: true};
  }

  if (!skipSource) {
    const frameworkDir = resolve(root, 'crates/cayw-visual-editor/framework');
    if (!(await pathExists(join(frameworkDir, 'node_modules')))) {
      const fwCi = run('npm', ['ci'], {cwd: frameworkDir});
      await writeFile(join(out, 'framework-npm-ci.log'), `${fwCi.stdout_tail}\n---stderr---\n${fwCi.stderr_tail}\n`);
      summary.steps.framework_npm_ci = {...fwCi, stdout_tail: undefined, stderr_tail: undefined};
      assert(fwCi.passed, 'npm ci in crates/cayw-visual-editor/framework failed');
    } else {
      summary.steps.framework_npm_ci = {skipped: true, reason: 'node_modules already present'};
    }
    const sourceTest = run('node', ['--test', 'crates/cayw-visual-editor/framework/source.test.mjs']);
    await writeFile(join(out, 'framework-source-test.log'), `${sourceTest.stdout_tail}\n---stderr---\n${sourceTest.stderr_tail}\n`);
    summary.steps.framework_source_test = {
      ...sourceTest,
      stdout_tail: undefined,
      stderr_tail: undefined,
    };
    assert(sourceTest.passed, 'framework source tests failed');
  } else {
    summary.steps.framework_source_test = {skipped: true};
  }

  if (!skipCargo) {
    const cargo = run('cargo', ['test', '-p', 'cayw-visual-editor', '--locked', '--', '--test-threads=4'], {
      env: {
        ...process.env,
        PATH: `${process.env.HOME}/.cargo/bin:${process.env.PATH || ''}`,
        CARGO_TERM_COLOR: 'never',
      },
    });
    await writeFile(join(out, 'cayw-visual-editor-cargo-test.log'), `${cargo.stdout_tail}\n---stderr---\n${cargo.stderr_tail}\n`);
    summary.steps.cargo_visual_editor_test = {
      ...cargo,
      stdout_tail: undefined,
      stderr_tail: undefined,
    };
    assert(cargo.passed, `cargo test -p cayw-visual-editor failed (status=${cargo.status})`);
  } else {
    summary.steps.cargo_visual_editor_test = {skipped: true};
  }

  if (probe) {
    const baseRaw = (process.env.MAGPIE_BASE_URL || 'http://127.0.0.1:3425/v1').replace(/\/$/, '');
    let helloUrl;
    let modelsUrl;
    try {
      const u = new URL(baseRaw.includes('://') ? baseRaw : `http://${baseRaw}`);
      const rootOrigin = `${u.protocol}//${u.host}`;
      helloUrl = `${rootOrigin}/hello`;
      modelsUrl = baseRaw.endsWith('/v1') ? `${baseRaw}/models` : `${rootOrigin}/v1/models`;
    } catch {
      helloUrl = 'http://127.0.0.1:3425/hello';
      modelsUrl = 'http://127.0.0.1:3425/v1/models';
    }
    summary.steps.magpie_public_probes = {
      hello: await publicProbe(helloUrl, 'public-hello', out),
      models: await publicProbe(modelsUrl, 'public-models', out),
      authorization_sent: false,
      note: 'Public probes do not establish Magpie generation acceptance.',
    };
  }

  summary.finished_utc = new Date().toISOString();
  summary.passed =
    summary.projects.react.passed &&
    summary.projects.vue.passed &&
    (skipNpm || (summary.steps.react_npm?.passed && summary.steps.vue_npm?.passed)) &&
    (skipEditor || (summary.steps.react_editor_chain?.passed && summary.steps.vue_editor_chain?.passed)) &&
    (skipSource || summary.steps.framework_source_test?.passed) &&
    (skipCargo || summary.steps.cargo_visual_editor_test?.passed);

  summary.chinese_ready_facts = {
    slice: 'L',
    feature: 'F18',
    范围: '外部/夹具 React·Vue：结构检查 + 可选构建 + 编辑器 HTTP open/edit/Host-save/reopen；不宣称 Magpie 生成通过',
    editor_chain: 'cayw-visual-editor /api/document → /api/operation → /api/save（Host fs.write）→ 停服重启再 /api/document',
    linux_填补: '对传入工程路径（默认仓内 fixtures 副本）跑真实打开编辑保存再打开；缺 Issue-30 fixture id 的外部工程不再误失败',
    magpie_状态: summary.magpie_generation.status,
    magpie_由本脚本确立: false,
    凭据映射: 'scripts/load-gateway-magpie-env.sh 将 /workspace/.env 的 GATEWAY_URL/GATEWAY_KEY 映射为 MAGPIE_BASE_URL/MAGPIE_API_KEY，证据中只记录 present/absent',
    macos_Magpie入口: 'source scripts/load-gateway-magpie-env.sh && node scripts/validate-visual-editor-magpie.mjs --out NEW_DIR',
    本轮通过: summary.passed,
  };

  await writeFile(join(out, 'summary.json'), `${JSON.stringify(summary, null, 2)}\n`);
  console.log(JSON.stringify({
    passed: summary.passed,
    out,
    magpie_generation: summary.magpie_generation.status,
    magpie_established_by_this_script: false,
    react_origin: summary.projects.react.source_origin,
    vue_origin: summary.projects.vue.source_origin,
    react_editor_chain: summary.steps.react_editor_chain?.passed ?? summary.steps.editor_chain,
    vue_editor_chain: summary.steps.vue_editor_chain?.passed ?? summary.steps.editor_chain,
    credentials: summary.credentials,
  }, null, 2));
  process.exit(summary.passed ? 0 : 1);
}

main().catch((error) => {
  console.error(String(error?.stack || error));
  process.exit(error?.code === 2 ? 2 : 1);
});
