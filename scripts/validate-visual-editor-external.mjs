#!/usr/bin/env node
/**
 * F18 / Issue #30 — external React/Vue project acceptance harness (SLICE L).
 *
 * Purpose
 * -------
 * Fill the Magpie/darwin-blocked gap on Linux (and any non-darwin host) by
 * running a fixture-backed, non-browser acceptance path against real-shaped
 * Vite React / Vue projects. Optionally accept caller-supplied project roots
 * that look like external apps (package.json + Vite + framework sources).
 *
 * This script does NOT:
 *   - send Magpie generation requests on non-darwin (darwin Magpie path is
 *     documented separately; use validate-visual-editor-magpie.mjs there)
 *   - print secrets (MAGPIE_API_KEY / GATEWAY_KEY values)
 *   - claim full browser Playwright acceptance (that remains macOS-scoped)
 *
 * Exit codes
 * ----------
 *   0  fixture/external structural + build + package tests passed
 *   1  a required check failed
 *   2  usage / configuration error
 */
import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {access, cp, mkdir, readFile, readdir, rm, stat, writeFile} from 'node:fs/promises';
import {constants as fsConstants} from 'node:fs';
import {tmpdir, release} from 'node:os';
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
  --skip-npm-build          skip npm ci / npm run build for each project
  --skip-cargo-test         skip cargo test -p cayw-visual-editor --locked
  --skip-source-test        skip node --test framework/source.test.mjs
  --probe-magpie            unauthenticated GET /hello and GET /v1/models (no key sent)
  --help

Credentials are never printed. For macOS Magpie generation, see
docs/validation/f18-external-project-validation.md and
scripts/load-gateway-magpie-env.sh + scripts/validate-visual-editor-magpie.mjs.`);
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
    // Cap digest map to keep evidence small; still list count.
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
    const token = sel.startsWith('#') ? `id="${sel.slice(1)}"` : sel;
    const alt = sel.startsWith('#') ? `id='${sel.slice(1)}'` : sel;
    const hit = joined.includes(token) || joined.includes(alt) || joined.includes(sel.slice(1));
    selectorHits[sel] = hit;
    if (!hit) missing += 1;
  }
  result.selector_hits = selectorHits;
  result.checks.push({
    name: 'required-selectors-in-source',
    passed: missing === 0,
    missing,
  });
  // External projects may not use the Issue-30 fixture ids; only require them for fixtures.
  if (fixtureMark) {
    assert(missing === 0, `${label}: fixture missing ${missing} required selectors in source`);
  } else {
    result.checks.push({
      name: 'external-selector-requirement',
      passed: true,
      note: 'Caller-supplied projects are not required to use Issue-30 fixture ids; structural Vite/framework checks still apply.',
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
  const probe = flag('--probe-magpie');

  const work = join(out, 'work');
  await mkdir(work, {recursive: true});

  const reactSrc = option('--react-project')
    ? resolve(option('--react-project'))
    : resolve(root, FIXTURES.react);
  const vueSrc = option('--vue-project')
    ? resolve(option('--vue-project'))
    : resolve(root, FIXTURES.vue);

  // Copy into work/ so npm installs never dirty the fixture originals or caller trees unexpectedly.
  // Caller-supplied external projects are copied as well for isolated builds.
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
    schema: 'cayw.f18.external-project-validation.v1',
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
      macos_driver: 'scripts/validate-visual-editor-magpie.mjs',
      credential_loader: 'scripts/load-gateway-magpie-env.sh',
      note: 'Linux/non-darwin hosts fill the Magpie gap with fixture/external structural+build checks only. Real Magpie generation remains macOS-scoped.',
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
    const origin = new URL(baseRaw.endsWith('/v1') ? baseRaw.slice(0, -3) || baseRaw : baseRaw);
    // Prefer MAGPIE_BASE_URL host; fall back to constructing from URL.
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
    };
  }

  summary.finished_utc = new Date().toISOString();
  summary.passed =
    summary.projects.react.passed &&
    summary.projects.vue.passed &&
    (skipNpm || (summary.steps.react_npm?.passed && summary.steps.vue_npm?.passed)) &&
    (skipSource || summary.steps.framework_source_test?.passed) &&
    (skipCargo || summary.steps.cargo_visual_editor_test?.passed);

  summary.chinese_ready_facts = {
    slice: 'L',
    feature: 'F18',
    范围: '外部/夹具 React·Vue 工程的结构、构建与可视化编辑器包测；不在非 darwin 上宣称 Magpie 真实生成通过',
    linux_填补: '用仓内 fixtures 与可选外部工程路径，完成 npm 构建 + framework source 测试 + cayw-visual-editor cargo 测试',
    magpie_状态: summary.magpie_generation.status,
    凭据映射: 'scripts/load-gateway-magpie-env.sh 将 /workspace/.env 的 GATEWAY_URL/GATEWAY_KEY 映射为 MAGPIE_BASE_URL/MAGPIE_API_KEY，证据中只记录 present/absent',
    macos_Magpie入口: 'source scripts/load-gateway-magpie-env.sh && node scripts/validate-visual-editor-magpie.mjs --out NEW_DIR',
    本轮通过: summary.passed,
  };

  await writeFile(join(out, 'summary.json'), `${JSON.stringify(summary, null, 2)}\n`);
  console.log(JSON.stringify({
    passed: summary.passed,
    out,
    magpie_generation: summary.magpie_generation.status,
    react_origin: summary.projects.react.source_origin,
    vue_origin: summary.projects.vue.source_origin,
    credentials: summary.credentials,
  }, null, 2));
  process.exit(summary.passed ? 0 : 1);
}

main().catch((error) => {
  console.error(String(error?.stack || error));
  process.exit(error?.code === 2 ? 2 : 1);
});
