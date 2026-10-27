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
import {access, cp, mkdir, readFile, readdir, rename, writeFile} from 'node:fs/promises';
import {appendFileSync, constants as fsConstants} from 'node:fs';
import {release} from 'node:os';
import {dirname, join, resolve, relative} from 'node:path';
import {fileURLToPath} from 'node:url';

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
const activeChildren = new Set();
const childResults = [];
const runAbort = new AbortController();
let evidenceOut;
let summary;

function errorRecord(error) {
  return {message: String(error?.message || error), code: error?.code ?? null, cleanup: error?.cleanup ?? null};
}

async function writeJson(path, value) {
  const temporary = `${path}.tmp`;
  await writeFile(temporary, `${JSON.stringify(value, null, 2)}\n`);
  await rename(temporary, path);
}

async function checkpoint(stage) {
  summary.stage = stage;
  summary.updated_utc = new Date().toISOString();
  await writeJson(join(evidenceOut, 'summary.json'), summary);
  console.log(JSON.stringify({stage, out: evidenceOut}));
  runAbort.signal.throwIfAborted();
}

function interrupted(signal) {
  if (runAbort.signal.aborted) return;
  const error = Object.assign(new Error(`validation interrupted by ${signal}`), {code: signal});
  runAbort.abort(error);
}

process.on('SIGINT', () => interrupted('SIGINT'));
process.on('SIGTERM', () => interrupted('SIGTERM'));

function processTable() {
  const result = spawnSync('/bin/ps', ['-axo', 'pid=,ppid=,pgid=,lstart=,command='], {
    encoding: 'utf8',
    env: {...process.env, LC_ALL: 'C'},
  });
  assert(result.status === 0, 'could not inspect owned child processes');
  return result.stdout.trim().split('\n').filter(Boolean).map((line) => {
    const fields = line.trim().split(/\s+/);
    return {
      pid: Number(fields[0]), ppid: Number(fields[1]), pgid: Number(fields[2]),
      started: fields.slice(3, 8).join(' '), command: fields.slice(8).join(' '),
    };
  });
}

function sameProcess(a, b) {
  return a.pid === b.pid && a.started === b.started && a.command === b.command;
}

function ownedProcesses(run) {
  const table = processTable();
  if (!table.some((row) => row.pid === run.child.pid && row.ppid === process.pid)) return [];
  const pids = new Set([run.child.pid]);
  let changed;
  do {
    changed = false;
    for (const row of table) {
      if (pids.has(row.ppid) && !pids.has(row.pid)) { pids.add(row.pid); changed = true; }
    }
  } while (changed);
  return table.filter((row) => pids.has(row.pid));
}

function spawnRecorded(command, argv, name, outDir, opts = {}) {
  runAbort.signal.throwIfAborted();
  const child = spawn(command, argv, {
    cwd: opts.cwd || root,
    env: opts.env || process.env,
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: true,
  });
  const run = {
    child, command, argv, name, outDir, started_utc: new Date().toISOString(),
    stdout: '', stderr: '', status: null, finished: false, owned: [],
  };
  activeChildren.add(run);
  child.stdout.on('data', (chunk) => {
    run.stdout += chunk;
    appendFileSync(join(outDir, `${name}-stdout.log`), chunk);
  });
  child.stderr.on('data', (chunk) => {
    run.stderr += chunk;
    appendFileSync(join(outDir, `${name}-stderr.log`), chunk);
  });
  run.closed = new Promise((accept) => {
    child.on('error', (error) => { run.spawn_error = errorRecord(error); });
    child.on('exit', (code, signal) => { run.status = {code, signal}; });
    child.on('close', (code, signal) => {
      run.status = {code, signal};
      run.closed_at = new Date().toISOString();
      accept(run.status);
    });
  });
  return run;
}

async function bounded(promise, milliseconds, message, signal = undefined) {
  let timer;
  let onAbort;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(message)), milliseconds);
        if (signal) {
          onAbort = () => reject(signal.reason);
          if (signal.aborted) onAbort();
          else signal.addEventListener('abort', onAbort, {once: true});
        }
      }),
    ]);
  } finally {
    clearTimeout(timer);
    if (onAbort) signal.removeEventListener('abort', onAbort);
  }
}

async function stopChild(run) {
  if (run.stopping) return run.stopping;
  run.stopping = (async () => {
    const signals = [];
    if (!run.closed_at) {
      const observed = ownedProcesses(run);
      for (const row of observed) {
        if (!run.owned.some((owned) => sameProcess(owned, row))) run.owned.push(row);
      }
      if (run.status === null) {
        signals.push({target: 'owned-child', signal: 'SIGINT', accepted: run.child.kill('SIGINT')});
      }
      try {
        await bounded(run.closed, 15000, `${run.name}: shutdown timed out`);
      } catch (error) {
        run.shutdown_error = errorRecord(error);
        if (run.status === null) {
          signals.push({target: 'owned-child', signal: 'SIGKILL', accepted: run.child.kill('SIGKILL')});
        }
      }
    }
    // The companion owns a different process group. Only the descendants observed
    // under this child, with matching start time and command, can be cleaned here.
    let table = processTable();
    for (const owned of [...run.owned].reverse()) {
      if (owned.pid === run.child.pid) continue;
      if (!table.some((row) => sameProcess(row, owned))) continue;
      try {
        process.kill(owned.pid, 'SIGKILL');
        signals.push({target: owned, signal: 'SIGKILL', accepted: true});
      } catch (error) {
        if (error.code !== 'ESRCH') throw error;
      }
      table = processTable();
    }
    await bounded(run.closed, 5000, `${run.name}: child output did not close`);
    table = processTable();
    const remaining = run.owned.filter((owned) => table.some((row) => sameProcess(row, owned)));
    const result = {
      name: run.name, command: run.command, argv: run.argv, pid: run.child.pid ?? null,
      started_utc: run.started_utc, finished_utc: run.closed_at,
      exit: run.status, spawn_error: run.spawn_error ?? null,
      shutdown_error: run.shutdown_error ?? null, signals,
      observed_owned_processes: run.owned, remaining_owned_processes: remaining,
      cleanup_complete: remaining.length === 0,
    };
    await writeFile(join(run.outDir, `${run.name}-stdout.log`), run.stdout);
    await writeFile(join(run.outDir, `${run.name}-stderr.log`), run.stderr);
    await writeJson(join(run.outDir, `${run.name}-exit.json`), result);
    childResults.push(result);
    run.finished = true;
    activeChildren.delete(run);
    assert(result.cleanup_complete, `${run.name}: owned processes remain after cleanup`);
    return result;
  })();
  return run.stopping;
}

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
  --reuse-installed-dependencies  copy each input project's installed node_modules
  --offline                 require installed dependencies; never run npm ci
  --startup-timeout-ms N    editor readiness deadline (default: 120000)
  --skip-npm-build          skip npm run build for each project
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

async function runCheck(command, argv, name, outDir, opts = {}) {
  const childRun = spawnRecorded(command, argv, name, outDir, opts);
  let failure;
  try {
    await bounded(childRun.closed, 600000, `${name}: command timed out`, runAbort.signal);
  } catch (error) {
    failure = error;
  }
  const exit = await stopChild(childRun);
  if (failure) throw failure;
  const result = {...exit, passed: exit.exit.code === 0 && !exit.spawn_error};
  assert(result.passed, `${name} failed: ${JSON.stringify(exit.exit)}; see ${name}-stderr.log`);
  return result;
}

async function npmCommand(argv, name, projectPath, outDir) {
  // npm is executed by the same Node as this script, independent of shell PATH.
  const npmCli = resolve(dirname(process.execPath), '../lib/node_modules/npm/bin/npm-cli.js');
  assert(await pathExists(npmCli), `npm CLI missing beside ${process.execPath}`, 2);
  return runCheck(process.execPath, [npmCli, ...argv], name, outDir, {
    cwd: projectPath,
    env: {...process.env, PATH: `${dirname(process.execPath)}:${process.env.PATH || ''}`},
  });
}

async function npmBuild(label, projectPath, outDir) {
  const log = {label, project: projectPath, steps: []};
  log.steps.push({name: 'dependencies', ...await ensureNpmInstalled(label, projectPath, outDir)});
  const build = await npmCommand(['run', 'build'], `${label}-npm-build`, projectPath, outDir);
  log.steps.push({name: 'npm-build', ...build});
  log.dist_index_present = await pathExists(join(projectPath, 'dist', 'index.html'));
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
  assert(!flag('--offline'), `${label}: offline dependencies missing: ${join(projectPath, 'node_modules')}`, 2);
  const ci = await npmCommand(['ci'], `${label}-npm-ci`, projectPath, outDir);
  return {skipped: false, ...ci};
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

async function startEditor(binary, projectPath, stateDir, framework, name, outDir) {
  const argv = [
    '--project', projectPath,
    '--state-dir', stateDir,
    '--framework', framework,
    '--addr', '127.0.0.1:0',
    '--preview-addr', '127.0.0.1:0',
    '--node', process.execPath,
    '--companion', join(root, 'crates/cayw-visual-editor/framework/companion.mjs'),
  ];
  const childRun = spawnRecorded(binary, argv, name, outDir, {
    env: {...process.env, CAYW_EDITOR_TOKEN: EDITOR_TOKEN},
  });
  const ready = new Promise((accept, reject) => {
    childRun.child.on('error', reject);
    childRun.child.on('exit', (code, signal) => {
      reject(new Error(`${name} exited before ready: ${code}/${signal}; ${childRun.stderr.slice(-2000)}`));
    });
    childRun.child.stdout.on('data', () => {
      if (!childRun.stdout.includes('\n')) return;
      try {
        const urls = JSON.parse(childRun.stdout.split('\n')[0]);
        for (const field of ['editor_url', 'preview_url', 'normal_preview_url']) {
          const url = new URL(urls[field]);
          assert(url.protocol === 'http:' && url.hostname === '127.0.0.1', `${name}: invalid ${field}`);
        }
        accept(urls);
      } catch (error) { reject(error); }
    });
  });
  try {
    childRun.urls = await bounded(
      ready, summary.configuration.startup_timeout_ms, `${name}: editor readiness timed out`, runAbort.signal,
    );
    childRun.owned = ownedProcesses(childRun);
    await writeJson(join(outDir, `${name}-ready.json`), {
      urls: childRun.urls, argv, pid: childRun.child.pid, observed_owned_processes: childRun.owned,
    });
    return childRun;
  } catch (error) {
    try { await stopChild(childRun); } catch (cleanup) { error.cleanup = errorRecord(cleanup); }
    throw error;
  }
}

async function stopEditor(childRun) {
  const exit = await stopChild(childRun);
  assert(exit.exit.code === 0 && !exit.spawn_error, `${childRun.name} did not stop cleanly: ${JSON.stringify(exit.exit)}`);
  return exit;
}

async function apiGet(editorUrl, path) {
  const response = await fetch(`${editorUrl.replace(/\/$/, '')}${path}`, {
    headers: {Authorization: `Bearer ${EDITOR_TOKEN}`},
    signal: AbortSignal.any([runAbort.signal, AbortSignal.timeout(30000)]),
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
    signal: AbortSignal.any([runAbort.signal, AbortSignal.timeout(60000)]),
  });
  const body = await response.json().catch(() => null);
  return {status: response.status, body};
}

async function readAuditHostWrite(stateDir) {
  const raw = await readFile(join(stateDir, 'calls.jsonl'), 'utf8');
  const rows = raw.trim() ? raw.trim().split('\n').map((line) => JSON.parse(line)) : [];
  const writes = rows.filter((row) => row.event === 'started' && row.capability === 'fs.write');
  const settled = writes.map((write) => ({
    call_id: write.call_id,
    path: write.arguments.path,
    result: rows.find((row) => row.event === 'settled' && row.call_id === write.call_id),
  }));
  return {
    rows: rows.length,
    fs_write_events: writes.length,
    has_fs_write: writes.length > 0,
    all_writes_settled: settled.every((write) => write.result && !write.result.outcome_unknown && !write.result.result?.error),
    writes: settled,
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

  summary.steps[`${label}_editor_chain`] = result;
  async function record(step) {
    result.steps.push(step);
    await writeJson(join(outDir, `${label}-editor-chain.json`), result);
    await checkpoint(`${label}:${step.name}`);
  }
  try {
    assert(await pathExists(binary), `${label}: editor binary missing: ${binary}`, 2);
    const npm = await ensureNpmInstalled(label, projectPath, outDir);
    await record({name: 'ensure-npm', ...npm});

    const stateA = join(outDir, `${label}-editor-state-open`);
    const stateB = join(outDir, `${label}-editor-state-reopen`);
    await mkdir(stateA, {recursive: true});
    await mkdir(stateB, {recursive: true});

    const before = await digestTree(projectPath);
    await record({name: 'starting-open'});
    let run = await startEditor(binary, projectPath, stateA, framework, `${label}-editor-open`, outDir);
    await record({name: 'open', urls: run.urls, argv: run.argv});

    try {
      const doc = await apiGet(run.urls.editor_url, '/api/document');
      assert(doc.status === 200, `${label}: /api/document failed (${doc.status})`);
      assert(doc.body?.snapshot?.adapter === framework, `${label}: adapter mismatch`);
      await record({
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
      await record({
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
      assert(auditA.all_writes_settled, `${label}: Host fs.write did not settle with a known result`);
      await record({
        name: 'save',
        status: saved.status,
        changed_files: changed,
        host_audit: auditA,
        new_baseline_sha256: saved.body.snapshot?.sha256 || null,
      });
    } finally {
      await record({name: run.name.endsWith('reopen') ? 'stopped-reopen' : 'stopped-open', exit: await stopEditor(run)});
    }

    // Reopen: fresh editor process + state against the same project path.
    await record({name: 'starting-reopen'});
    run = await startEditor(binary, projectPath, stateB, framework, `${label}-editor-reopen`, outDir);
    await record({name: 'reopen', urls: run.urls, argv: run.argv});
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
      await record({
        name: 'document-reopen',
        status: again.status,
        dirty: again.body.dirty,
        retained_edit: true,
        adapter: again.body.snapshot.adapter,
      });
    } finally {
      await record({name: run.name.endsWith('reopen') ? 'stopped-reopen' : 'stopped-open', exit: await stopEditor(run)});
    }

    result.passed = true;
    result.status = 'passed';
    return result;
  } catch (error) {
    result.status = runAbort.signal.aborted ? 'interrupted' : 'failed';
    result.error = errorRecord(error);
    throw error;
  } finally {
    result.finished_utc = new Date().toISOString();
    await writeJson(join(outDir, `${label}-editor-chain.json`), result);
  }
}

async function main() {
  if (flag('--help')) { usage(); return; }
  const outArg = option('--out');
  assert(outArg, '--out requires a new evidence directory', 2);
  const out = resolve(outArg);
  assert(!(await pathExists(out)), `--out must be a new directory: ${out}`, 2);
  await mkdir(dirname(out), {recursive: true});
  await mkdir(out, {recursive: false});
  evidenceOut = out;
  summary = {
    schema: 'cayw.f18.external-project-validation.v3',
    slice: 'L', feature: 'F18', issue: 30,
    started_utc: new Date().toISOString(),
    platform: process.platform, architecture: process.arch, os_release: release(),
    node: process.version, node_executable: process.execPath,
    base_commit: run('git', ['rev-parse', 'HEAD']).stdout_tail.trim(),
    out, credentials: credentialPresence(),
    magpie_generation: {status: 'not_run_by_this_harness', established_by_this_script: false},
    projects: {}, steps: {}, status: 'running', passed: false,
  };
  await checkpoint('initializing');
  assert(process.platform === 'darwin', 'this validation entry is supported on macOS', 2);
  const skipNpm = flag('--skip-npm-build');
  const skipCargo = flag('--skip-cargo-test');
  const skipSource = flag('--skip-source-test');
  const skipEditor = flag('--skip-editor-chain');
  const reuse = flag('--reuse-installed-dependencies');
  const startupTimeout = Number(option('--startup-timeout-ms', '120000'));
  assert(Number.isSafeInteger(startupTimeout) && startupTimeout > 0, 'invalid --startup-timeout-ms', 2);
  const editorBinary = resolve(option('--editor-binary') || join(root, 'target/debug/cayw-visual-editor'));
  summary.configuration = {
    offline: flag('--offline'), reuse_installed_dependencies: reuse,
    skip_npm_build: skipNpm, skip_cargo_test: skipCargo,
    skip_source_test: skipSource, skip_editor_chain: skipEditor,
    public_probe_requested: flag('--probe-magpie'), startup_timeout_ms: startupTimeout,
    editor_binary: editorBinary,
  };
  summary.source_sha256 = {};
  for (const path of ['scripts/validate-visual-editor-external.mjs', 'crates/cayw-visual-editor/framework/companion.mjs', 'crates/cayw-visual-editor/framework/source.mjs']) {
    summary.source_sha256[path] = await shaFile(join(root, path));
  }
  if (!skipEditor) {
    assert(await pathExists(editorBinary), `editor binary missing: ${editorBinary}`, 2);
    summary.editor_binary_sha256 = await shaFile(editorBinary);
  }
  const work = join(out, 'work');
  await mkdir(work);
  const projects = {};
  for (const label of ['react', 'vue']) {
    const source = resolve(option(`--${label}-project`) || join(root, FIXTURES[label]));
    const project = join(work, `${label}-project`);
    projects[label] = project;
    summary.projects[label] = {source_path: source, path: project, passed: false};
    await checkpoint(`${label}:copying-input`);
    // Source and dependencies are independent copies; Host writes never touch inputs.
    await cp(source, project, {recursive: true, filter: (path) => {
      const base = path.split(/[/\\]/).pop();
      return base !== '.git' && base !== 'dist' && base !== 'node_modules';
    }});
    if (reuse) {
      assert(await pathExists(join(source, 'node_modules')), `${label}: requested installed dependencies missing`, 2);
      await cp(join(source, 'node_modules'), join(project, 'node_modules'), {
        recursive: true, verbatimSymlinks: true,
      });
    }
    summary.projects[label] = {...await inspectProject(label, project, label), source_path: source};
    if (reuse) summary.projects[label].installed_dependencies_source = join(source, 'node_modules');
    await checkpoint(`${label}:inspected`);
  }
  for (const label of ['react', 'vue']) {
    if (!skipNpm) {
      await checkpoint(`${label}:building`);
      summary.steps[`${label}_npm`] = await npmBuild(label, projects[label], out);
      await checkpoint(`${label}:built`);
    }
    if (!skipEditor) {
      await exerciseEditorChain(label, projects[label], label, editorBinary, out);
    }
  }
  if (skipNpm) summary.steps.npm = {skipped: true};
  if (skipEditor) summary.steps.editor_chain = {skipped: true};

  if (!skipSource) {
    await checkpoint('framework:source-test');
    const frameworkDir = join(root, 'crates/cayw-visual-editor/framework');
    summary.steps.framework_dependencies = await ensureNpmInstalled('framework', frameworkDir, out);
    summary.steps.framework_source_test = await runCheck(
      process.execPath, ['--test', 'crates/cayw-visual-editor/framework/source.test.mjs'],
      'framework-source-test', out,
    );
  } else {
    summary.steps.framework_source_test = {skipped: true};
  }
  if (!skipCargo) {
    await checkpoint('cargo:visual-editor-test');
    summary.steps.cargo_visual_editor_test = await runCheck(
      'cargo', ['test', '-p', 'cayw-visual-editor', '--locked', '--', '--test-threads=4'],
      'cayw-visual-editor-cargo-test', out, {
        env: {
          ...process.env,
          PATH: `${dirname(process.execPath)}:${process.env.HOME}/.cargo/bin:${process.env.PATH || ''}`,
          CARGO_TERM_COLOR: 'never',
        },
      },
    );
  } else {
    summary.steps.cargo_visual_editor_test = {skipped: true};
  }
  if (flag('--probe-magpie')) {
    await checkpoint('public-probe');
    const base = new URL(process.env.MAGPIE_BASE_URL || 'http://127.0.0.1:3425/v1');
    summary.steps.magpie_public_probes = {
      hello: await publicProbe(new URL('/hello', base).href, 'public-hello', out),
      models: await publicProbe(new URL('/v1/models', base).href, 'public-models', out),
      authorization_sent: false,
    };
  }
  runAbort.signal.throwIfAborted();
  summary.passed = true;
}

try {
  await main();
} catch (error) {
  console.error(String(error?.stack || error));
  process.exitCode = error?.code === 2 ? 2 : 1;
  if (summary) {
    summary.error = errorRecord(error);
    summary.passed = false;
  }
} finally {
  const cleanup = [];
  for (const childRun of [...activeChildren]) {
    try { await stopChild(childRun); }
    catch (error) { cleanup.push({name: childRun.name, cleanup_complete: false, error: errorRecord(error)}); }
  }
  if (summary) {
    summary.cleanup = [...childResults, ...cleanup];
    summary.cleanup_complete = summary.cleanup.every((entry) => entry.cleanup_complete);
    if (!summary.cleanup_complete || runAbort.signal.aborted) summary.passed = false;
    summary.status = runAbort.signal.aborted ? 'interrupted' : summary.passed ? 'passed' : 'failed';
    summary.finished_utc = new Date().toISOString();
    await writeJson(join(evidenceOut, 'summary.json'), summary);
    console.log(JSON.stringify({status: summary.status, passed: summary.passed, out: evidenceOut}));
    if (!summary.passed) process.exitCode ||= 1;
  }
}
