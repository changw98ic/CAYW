// Line-delimited JSON companion for the CAYW visual editor.
//
// The Rust host starts this process with a cleared environment, a state
// directory as its working directory, and piped stdin/stdout. Every request is
// one JSON line `{id,method,params}`; every response is one JSON line
// `{id,ok:true,result}` or `{id,ok:false,error:{code,message}}`. Diagnostics go
// to stderr only. The process never writes to the project; it serves in-memory
// drafts through Vite and reports source patches back to the Rust host.

import { readdir, realpath, stat } from 'node:fs/promises';
import { createServer as createTcpServer } from 'node:net';
import path from 'node:path';

const MAX_LINE_BYTES = 32 * 1024 * 1024;
const MAX_FILES = 256;
const MAX_SOURCE_BYTES = 16 * 1024 * 1024;
const MAX_FILE_BYTES = 8 * 1024 * 1024;
const DEFAULT_FS_DENY = ['.env', '.env.*', '*.{crt,pem}', '**/.git/**'];
const BRIDGE_PATH = '/__cayw_bridge.js';
const SOURCE_EXTENSIONS = new Set(['.js', '.jsx', '.ts', '.tsx', '.vue']);
const EXCLUDED_DIRECTORIES = new Set(['node_modules', 'dist', 'build', '.git']);

// Vite and a project config may log to stdout, which would corrupt the protocol.
// Route every console channel except stderr writes explicitly through stderr.
for (const channel of ['log', 'info', 'warn', 'debug', 'trace', 'dir']) {
  console[channel] = (...args) => {
    process.stderr.write(`${args.map(String).join(' ')}\n`);
  };
}

class CompanionError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'CompanionError';
    this.code = code;
  }
}

function coded(code, message) {
  return new CompanionError(code, message);
}

function requireString(value, name) {
  if (typeof value !== 'string' || value.length === 0) {
    throw coded('invalid_request', `${name} must be a non-empty string`);
  }
  return value;
}

function requireFramework(value) {
  if (value !== 'react' && value !== 'vue') {
    throw coded('invalid_request', 'framework must be "react" or "vue"');
  }
  return value;
}

function requireRevision(value) {
  requireString(value, 'revision');
  if (!/^[0-9a-f]{64}$/i.test(value)) {
    throw coded('invalid_request', 'revision must be one SHA-256 hex digest');
  }
  // Keep the exact spelling: node ids hash this string and the Rust host
  // compares the ids it receives with the revision it sent.
  return value;
}

function normalizeRelative(value, name) {
  requireString(value, name);
  if (value.includes('\0') || value.includes('\\')) {
    throw coded('invalid_request', `${name} must be a portable relative path`);
  }
  const normalized = path.posix.normalize(value);
  if (
    normalized === '.' ||
    normalized.startsWith('/') ||
    normalized === '..' ||
    normalized.startsWith('../') ||
    path.posix.isAbsolute(normalized)
  ) {
    throw coded('invalid_request', `${name} must stay inside the project root`);
  }
  return normalized;
}

function toPosix(value) {
  return value.split(path.sep).join('/');
}

function normalizeFiles(files) {
  if (!Array.isArray(files)) {
    throw coded('invalid_request', 'files must be an array');
  }
  if (files.length > MAX_FILES) {
    throw coded('invalid_request', `files exceeds the ${MAX_FILES}-file limit`);
  }
  const seen = new Set();
  let total = 0;
  const output = [];
  for (const file of files) {
    if (file === null || typeof file !== 'object') {
      throw coded('invalid_request', 'each file must be an object');
    }
    const relative = normalizeRelative(file.path, 'file.path');
    if (typeof file.source !== 'string') {
      throw coded('invalid_request', `file.source for ${relative} must be a string`);
    }
    if (seen.has(relative)) {
      throw coded('invalid_request', `duplicate file path ${relative}`);
    }
    seen.add(relative);
    const bytes = Buffer.byteLength(file.source, 'utf8');
    if (bytes > MAX_FILE_BYTES) {
      throw coded('invalid_request', `${relative} exceeds the ${MAX_FILE_BYTES}-byte file limit`);
    }
    total += bytes;
    if (total > MAX_SOURCE_BYTES) {
      throw coded('invalid_request', `files exceeds the ${MAX_SOURCE_BYTES}-byte limit`);
    }
    output.push({ path: relative, source: file.source });
  }
  return output;
}

async function requireProjectRoot(value) {
  const root = await realpath(requireString(value, 'root')).catch(() => null);
  if (root === null) {
    throw coded('invalid_request', 'root must be an existing directory');
  }
  const details = await stat(root).catch(() => null);
  if (details === null || !details.isDirectory()) {
    throw coded('invalid_request', 'root must be an existing directory');
  }
  return root;
}

async function requireStateDir(value) {
  const stateDir = await realpath(requireString(value, 'state_dir')).catch(() => null);
  if (stateDir === null) {
    throw coded('invalid_request', 'state_dir must be an existing directory');
  }
  const details = await stat(stateDir).catch(() => null);
  if (details === null || !details.isDirectory()) {
    throw coded('invalid_request', 'state_dir must be an existing directory');
  }
  return stateDir;
}

function parseLoopbackAddress(value, name) {
  const address = requireString(value, name);
  let parsed;
  try {
    parsed = new URL(`http://${address}`);
  } catch {
    throw coded('invalid_request', `${name} must be a numeric loopback SocketAddr`);
  }
  const host = parsed.hostname.replace(/^\[|\]$/g, '');
  if (host !== '127.0.0.1' && host !== '::1') {
    throw coded('invalid_request', `${name} must use 127.0.0.1 or ::1`);
  }
  if (parsed.pathname !== '/' || parsed.search || parsed.hash) {
    throw coded('invalid_request', `${name} must be a numeric host and port`);
  }
  if (parsed.port === '' || parsed.username !== '' || parsed.password !== '') {
    throw coded('invalid_request', `${name} must contain one numeric host and port`);
  }
  const port = Number(parsed.port);
  if (!Number.isInteger(port) || port < 0 || port > 65535) {
    throw coded('invalid_request', `${name} must contain a valid port`);
  }
  return { host, port };
}

function validateEditorOrigin(value) {
  requireString(value, 'editor_origin');
  let parsed;
  try {
    parsed = new URL(value);
  } catch {
    throw coded('invalid_request', 'editor_origin must be an absolute loopback origin');
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    throw coded('invalid_request', 'editor_origin must use http or https');
  }
  const host = parsed.hostname;
  const loopback =
    host === '127.0.0.1' ||
    host === '::1' ||
    host === '[::1]' ||
    host === 'localhost' ||
    /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(host);
  if (!loopback) {
    throw coded('invalid_request', 'editor_origin must be a loopback origin');
  }
  if (parsed.pathname !== '/' || parsed.search || parsed.hash) {
    throw coded('invalid_request', 'editor_origin must not contain a path or query');
  }
  return `${parsed.protocol}//${parsed.host}`;
}

function socketAddress(host, port) {
  return host.includes(':') ? `[${host}]:${port}` : `${host}:${port}`;
}

function originFor(host, port) {
  return `http://${socketAddress(host, port)}`;
}

function basePathFrom(base) {
  if (typeof base !== 'string' || base.length === 0 || base === './') return '/';
  if (base.startsWith('http://') || base.startsWith('https://')) {
    const parsed = new URL(base);
    return parsed.pathname.endsWith('/') ? parsed.pathname : `${parsed.pathname}/`;
  }
  const pathname = base.startsWith('/') ? base : `/${base}`;
  return pathname.endsWith('/') ? pathname : `${pathname}/`;
}

function pageUrl(origin, basePath, page) {
  const encoded = page
    .split('/')
    .map((part) => encodeURIComponent(part))
    .join('/');
  return `${origin}${basePath}${encoded}`;
}

// ------------------------------------------------------------------ source API

let sourceModule = null;

async function sourceApi() {
  if (sourceModule !== null) return sourceModule;
  try {
    sourceModule = await import(new URL('./source.mjs', import.meta.url));
  } catch (error) {
    throw coded(
      'source_module_unavailable',
      `framework/source.mjs could not be loaded: ${error.message}`,
    );
  }
  for (const name of ['mapDocuments', 'editDocuments']) {
    if (typeof sourceModule[name] !== 'function') {
      throw coded(
        'source_module_unavailable',
        `framework/source.mjs must export ${name}()`,
      );
    }
  }
  return sourceModule;
}

function mapDrafts(api, framework, files, revision) {
  const mapped = api.mapDocuments({ framework, files, revision });
  if (mapped === null || typeof mapped !== 'object' || !Array.isArray(mapped.nodes)) {
    throw coded('parse_failed', 'mapDocuments did not return nodes');
  }
  const instrumented = mapped.instrumented ?? {};
  if (instrumented === null || typeof instrumented !== 'object') {
    throw coded('parse_failed', 'mapDocuments returned invalid instrumentation');
  }
  const drafts = new Map();
  for (const file of files) drafts.set(file.path, file.source);
  const overlays = new Map();
  for (const [relative, source] of Object.entries(instrumented)) {
    const normalized = normalizeRelative(relative, 'instrumented path');
    if (!drafts.has(normalized)) {
      throw coded('parse_failed', `instrumentation was returned for unknown file ${normalized}`);
    }
    if (typeof source !== 'string') {
      throw coded('parse_failed', `instrumentation for ${normalized} must be a string`);
    }
    overlays.set(normalized, source);
  }
  return { nodes: mapped.nodes, drafts, overlays };
}

// ---------------------------------------------------------------------- scan

async function scan(params) {
  const root = await requireProjectRoot(params.root);
  const page = normalizeRelative(params.page, 'page');
  const framework = requireFramework(params.framework);
  const files = [];
  let total = 0;

  async function add(relative) {
    const absolute = path.join(root, ...relative.split('/'));
    const details = await stat(absolute).catch(() => null);
    if (details === null || !details.isFile()) return;
    if (files.includes(relative)) return;
    files.push(relative);
    total += details.size;
    if (files.length > MAX_FILES) {
      throw coded('scan_limit', `project exceeds the ${MAX_FILES}-file scan limit`);
    }
    if (total > MAX_SOURCE_BYTES) {
      throw coded('scan_limit', `project exceeds the ${MAX_SOURCE_BYTES}-byte scan limit`);
    }
  }

  // The configured page is always part of the manifest even when it lives in a
  // directory the walk otherwise skips (for example a hidden directory).
  await add(page);

  async function walk(directory, prefix) {
    let entries;
    try {
      entries = await readdir(directory, { withFileTypes: true });
    } catch (error) {
      throw coded('scan_failed', `could not read ${prefix || '.'}: ${error.message}`);
    }
    entries.sort((left, right) => (left.name < right.name ? -1 : left.name > right.name ? 1 : 0));
    for (const entry of entries) {
      const name = entry.name;
      const relative = prefix === '' ? name : `${prefix}/${name}`;
      if (entry.isSymbolicLink()) continue;
      if (entry.isDirectory()) {
        if (name.startsWith('.') || EXCLUDED_DIRECTORIES.has(name)) continue;
        await walk(path.join(directory, name), relative);
        continue;
      }
      if (!entry.isFile()) continue;
      if (!SOURCE_EXTENSIONS.has(path.extname(name).toLowerCase())) continue;
      await add(relative);
    }
  }

  await walk(root, '');
  files.sort();
  // `framework` is part of the contract even though the extension filter is the
  // same for both supported frameworks.
  void framework;
  return { paths: files };
}

// --------------------------------------------------------------------- parse

async function parse(params) {
  const framework = requireFramework(params.framework);
  const files = normalizeFiles(params.files);
  const revision = requireRevision(params.revision);
  const api = await sourceApi();
  try {
    return api.mapDocuments({ framework, files, revision });
  } catch (error) {
    throw coded(error.code ?? 'parse_failed', error.message);
  }
}

// ---------------------------------------------------------------------- edit

async function edit(params) {
  const framework = requireFramework(params.framework);
  const files = normalizeFiles(params.files);
  const revision = requireRevision(params.revision);
  const operation = params.operation;
  if (operation === null || typeof operation !== 'object' || Array.isArray(operation)) {
    throw coded('invalid_request', 'operation must be an object');
  }
  const componentHtml = params.componentHtml;
  if (operation.kind === 'insert' && typeof componentHtml !== 'string') {
    throw coded('invalid_request', 'componentHtml is required for insert');
  }
  if (componentHtml !== undefined && typeof componentHtml !== 'string') {
    throw coded('invalid_request', 'componentHtml must be a string');
  }
  const api = await sourceApi();
  try {
    return api.editDocuments({ framework, files, revision, operation, componentHtml });
  } catch (error) {
    throw coded(error.code ?? 'edit_failed', error.message);
  }
}

// ------------------------------------------------------------------- preview

function pluginList(plugins) {
  if (!Array.isArray(plugins)) return [];
  return plugins.flat(Infinity).filter((plugin) => plugin !== null && plugin !== undefined && plugin !== false);
}

async function loadProjectConfig(root) {
  const { loadConfigFromFile } = await import('vite');
  try {
    const loaded = await loadConfigFromFile(
      { command: 'serve', mode: 'development', isSsrBuild: false, isPreview: false },
      undefined,
      root,
      'error',
    );
    return loaded?.config ?? {};
  } catch (error) {
    throw coded('preview_failed', `project Vite config could not be loaded: ${error.message}`);
  }
}

function resolveViteRoot(projectRoot, config) {
  const configured = config.root;
  if (typeof configured !== 'string' || configured.length === 0) return projectRoot;
  const resolved = path.isAbsolute(configured) ? configured : path.resolve(projectRoot, configured);
  const relative = path.relative(projectRoot, resolved);
  if (relative.startsWith('..') || path.isAbsolute(relative)) {
    throw coded('preview_failed', `the Vite root ${resolved} is outside the project root`);
  }
  return resolved;
}

function pageRelativeTo(viteRoot, projectRoot, page) {
  const absolute = path.resolve(projectRoot, ...page.split('/'));
  const relative = toPosix(path.relative(viteRoot, absolute));
  if (relative === '' || relative.startsWith('../') || path.isAbsolute(relative)) {
    throw coded('preview_failed', `page ${page} is outside the Vite root ${viteRoot}`);
  }
  return relative;
}

function serverOptions(project, projectRoot, { host, port, strictPort, edit }) {
  const source =
    project !== null && typeof project.server === 'object' && project.server !== null
      ? project.server
      : {};
  const projectFs =
    source.fs !== null && typeof source.fs === 'object' ? source.fs : {};
  const deny = [
    ...new Set([...DEFAULT_FS_DENY, ...(Array.isArray(projectFs.deny) ? projectFs.deny : [])]),
  ];
  const options = {
    ...source,
    host,
    port,
    strictPort,
    // The editor always speaks http to numeric loopback origins.
    https: false,
    hmr: false,
    open: false,
    fs: { ...projectFs, strict: true, allow: [projectRoot], deny },
  };
  if (edit) {
    // The edit preview only changes through `sync`; a disk watcher must never
    // replace a draft with the baseline that is still saved on disk. Keep the
    // project's own ignore list so a framework plugin that relies on the
    // watcher's ready state still works.
    options.watch = { ...(source.watch ?? {}) };
  }
  return options;
}

async function createProjectServer({
  project,
  projectRoot,
  viteRoot,
  host,
  port,
  strictPort,
  edit,
  plugins,
  cacheDir,
}) {
  const { createServer, mergeConfig } = await import('vite');
  const merged = mergeConfig(project, {
    root: viteRoot,
    configFile: false,
    cacheDir,
    mode: 'development',
  });
  merged.root = viteRoot;
  merged.configFile = false;
  merged.cacheDir = cacheDir;
  merged.mode = 'development';
  merged.server = serverOptions(project, projectRoot, { host, port, strictPort, edit });
  merged.plugins = plugins;
  const server = await createServer(merged);
  await server.listen();
  return server;
}

function listeningPort(server, expected) {
  const address = server.httpServer?.address();
  if (address === null || typeof address !== 'object' || typeof address === 'string') {
    throw coded('preview_failed', 'Vite did not report a listening address');
  }
  if (!Number.isInteger(address.port) || address.port === 0) {
    throw coded('preview_failed', 'Vite reported an invalid listening port');
  }
  if (expected !== undefined && address.port !== expected) {
    throw coded(
      'preview_failed',
      `Vite listened on port ${address.port} instead of the requested ${expected}`,
    );
  }
  return address.port;
}

// Vite treats `port: 0` as "use 5173", then falls back to the next free port.
// The editor contract requires an OS-assigned port and no fallback, so reserve
// one from the kernel and hand Vite an exact port with strictPort.
async function freeLoopbackPort(host) {
  return await new Promise((resolve, reject) => {
    const server = createTcpServer();
    server.unref();
    server.once('error', reject);
    server.listen(0, host, () => {
      const address = server.address();
      const port = typeof address === 'object' && address !== null ? address.port : 0;
      server.close(() => {
        if (Number.isInteger(port) && port > 0) {
          resolve(port);
        } else {
          reject(coded('preview_failed', 'could not reserve a loopback port'));
        }
      });
    });
  });
}

function bridgeSource(editorOrigin, basePath) {
  return `${basePath}__cayw_bridge.js?editor_origin=${encodeURIComponent(editorOrigin)}`;
}

function injectBridge(html, src) {
  if (html.includes('__cayw_bridge.js')) return html;
  const tag = `<script src="${src.replace(/&/g, '&amp;').replace(/"/g, '&quot;')}"></script>`;
  const head = /<head\b[^>]*>/i.exec(html);
  if (head !== null) {
    const at = head.index + head[0].length;
    return `${html.slice(0, at)}${tag}${html.slice(at)}`;
  }
  const root = /<html\b[^>]*>/i.exec(html);
  if (root !== null) {
    const at = root.index + root[0].length;
    return `${html.slice(0, at)}${tag}${html.slice(at)}`;
  }
  return `${tag}${html}`;
}

function htmlRequestPath(requestPath, basePath) {
  let value = requestPath ?? '/';
  if (basePath !== '/' && value.startsWith(basePath)) {
    value = value.slice(basePath.length - 1);
  }
  const clean = value.split('?')[0].split('#')[0];
  return clean.replace(/^\/+/, '');
}

function normalizeRequestPath(url) {
  try {
    return new URL(url, 'http://127.0.0.1').pathname;
  } catch {
    return '/';
  }
}

function createDraftPlugin({
  projectRoot,
  page,
  drafts,
  overlays,
  bridgeJs,
  editorOrigin,
  basePath,
}) {
  const bridgeSrc = bridgeSource(editorOrigin, basePath);
  return {
    name: 'cayw-visual-draft',
    enforce: 'pre',
    configureServer(server) {
      server.middlewares.use((request, response, next) => {
        const pathname = normalizeRequestPath(request.url ?? '/');
        if (pathname !== BRIDGE_PATH && pathname !== `${basePath}__cayw_bridge.js`) {
          next();
          return;
        }
        if (request.method !== 'GET' && request.method !== 'HEAD') {
          response.statusCode = 405;
          response.setHeader('Allow', 'GET, HEAD');
          response.end();
          return;
        }
        response.statusCode = 200;
        response.setHeader('Content-Type', 'text/javascript; charset=utf-8');
        response.setHeader('Cache-Control', 'no-store');
        response.setHeader('X-Content-Type-Options', 'nosniff');
        response.end(request.method === 'HEAD' ? undefined : bridgeJs);
      });
    },
    load(id) {
      if (id.includes('\0')) return null;
      const queryIndex = id.indexOf('?');
      // Vue/React virtual sub-modules (`?vue&type=...`, `?raw`, `?url`) must be
      // served by the project's own framework plugin from the descriptor it
      // built for the instrumented main module. Only cache-busting queries may
      // still select the draft.
      if (queryIndex !== -1) {
        const query = new URLSearchParams(id.slice(queryIndex + 1));
        for (const key of query.keys()) {
          if (key !== 't' && key !== 'import' && key !== 'direct') return null;
        }
      }
      const file = queryIndex === -1 ? id : id.slice(0, queryIndex);
      const relative = toPosix(path.relative(projectRoot, file));
      if (relative.startsWith('../') || path.isAbsolute(relative)) return null;
      // The instrumented document is what gives elements their data-cayw-node
      // marker. It must win over the plain draft.
      return overlays.get(relative) ?? drafts.get(relative) ?? null;
    },
    transformIndexHtml: {
      order: 'pre',
      handler(html, context) {
        const requestPath = htmlRequestPath(context?.path, basePath);
        const expected = page.replace(/^\/+/, '');
        if (requestPath !== expected && !(expected === 'index.html' && requestPath === '')) {
          return html;
        }
        return injectBridge(html, bridgeSrc);
      },
    },
  };
}

let previewState = null;

async function startPreview(params) {
  if (previewState !== null) {
    throw coded('preview_already_started', 'the preview servers are already running');
  }
  const root = await requireProjectRoot(params.root);
  const stateDir = await requireStateDir(params.state_dir);
  const page = normalizeRelative(params.page, 'page');
  const framework = requireFramework(params.framework);
  const editorOrigin = validateEditorOrigin(params.editor_origin);
  const requested = parseLoopbackAddress(params.preview_addr, 'preview_addr');
  if (typeof params.bridge_js !== 'string' || params.bridge_js.length === 0) {
    throw coded('invalid_request', 'bridge_js must be the preview bridge source');
  }
  const files = normalizeFiles(params.files);
  const revision = requireRevision(params.revision);
  const api = await sourceApi();
  let mapped;
  try {
    mapped = mapDrafts(api, framework, files, revision);
  } catch (error) {
    throw coded(error.code ?? 'parse_failed', error.message);
  }

  if (stateDir === root || stateDir.startsWith(`${root}${path.sep}`)) {
    throw coded('invalid_request', 'state_dir must be outside the project root');
  }
  const pageFile = path.resolve(root, ...page.split('/'));
  const pageDetails = await stat(pageFile).catch(() => null);
  if (pageDetails === null || !pageDetails.isFile()) {
    throw coded('preview_failed', `page ${page} is not a regular file`);
  }

  const { drafts, overlays } = mapped;
  const editCache = path.join(stateDir, 'vite-edit');
  const normalCache = path.join(stateDir, 'vite-normal');
  let editServer = null;
  let normalServer = null;
  let editPage = page;
  let normalPage = page;
  let editBase = '/';
  let normalBase = '/';
  let editExpectedPort;
  let normalExpectedPort;
  try {
    // Load the project config twice so each Vite server owns independent
    // framework plugin instances. Sharing instances would let one server reuse
    // a descriptor or transform cache built from the other server's content.
    const editConfig = await loadProjectConfig(root);
    const normalConfig = await loadProjectConfig(root);
    const editRoot = resolveViteRoot(root, editConfig);
    const normalRoot = resolveViteRoot(root, normalConfig);
    editPage = pageRelativeTo(editRoot, root, page);
    normalPage = pageRelativeTo(normalRoot, root, page);
    editBase = basePathFrom(editConfig.base);
    normalBase = basePathFrom(normalConfig.base);
    // An explicit requested port must bind exactly; a zero port means the
    // kernel assigns one. Vite's own zero handling would default to 5173 and
    // silently fall back, which the contract forbids.
    editExpectedPort =
      requested.port === 0 ? await freeLoopbackPort(requested.host) : requested.port;
    const draftPlugin = createDraftPlugin({
      projectRoot: root,
      page: editPage,
      drafts,
      overlays,
      bridgeJs: params.bridge_js,
      editorOrigin,
      basePath: editBase,
    });
    editServer = await createProjectServer({
      project: editConfig,
      projectRoot: root,
      viteRoot: editRoot,
      host: requested.host,
      port: editExpectedPort,
      strictPort: true,
      edit: true,
      plugins: [draftPlugin, ...pluginList(editConfig.plugins)],
      cacheDir: editCache,
    });
    normalExpectedPort = await freeLoopbackPort(requested.host);
    normalServer = await createProjectServer({
      project: normalConfig,
      projectRoot: root,
      viteRoot: normalRoot,
      host: requested.host,
      port: normalExpectedPort,
      strictPort: true,
      edit: false,
      plugins: pluginList(normalConfig.plugins),
      cacheDir: normalCache,
    });
  } catch (error) {
    await closeQuietly(editServer);
    await closeQuietly(normalServer);
    process.stderr.write(
      `companion: preview start failed: ${error instanceof Error ? error.stack : String(error)}\n`,
    );
    throw error instanceof CompanionError
      ? error
      : coded('preview_failed', `Vite preview could not start: ${error.message}`);
  }

  const editPort = listeningPort(editServer, editExpectedPort);
  const normalPort = listeningPort(normalServer, normalExpectedPort);
  const editOrigin = originFor(requested.host, editPort);
  const normalOrigin = originFor(requested.host, normalPort);

  previewState = {
    editServer,
    normalServer,
    root,
    page,
    framework,
    revision,
    drafts,
    overlays,
    basePath: editBase,
  };

  return {
    preview_url: pageUrl(editOrigin, editBase, editPage),
    preview_origin: editOrigin,
    preview_addr: socketAddress(requested.host, editPort),
    normal_preview_url: pageUrl(normalOrigin, normalBase, normalPage),
    normal_preview_origin: normalOrigin,
  };
}

async function closeQuietly(server, label = 'preview') {
  if (server === null) return;
  try {
    // Vite's environment close waits for every in-flight transform. A module
    // request still waiting on the dependency optimizer can never settle once
    // that optimizer is closed, and the close would hang. Abort what is
    // pending: the preview is going away and no client result is needed.
    for (const environment of Object.values(server.environments ?? {})) {
      for (const pending of environment._pendingRequests?.values() ?? []) {
        pending.abort?.();
      }
    }
    // A browser or probe keeps HTTP keep-alive sockets open. `http.Server.close`
    // waits for them, so Vite's close would never settle and shutdown would
    // time out. Ask Vite to close, then drop every connection it still holds.
    // closeServers() also bounds the whole wait, because a dependency
    // optimization that is still running can keep Vite's close pending.
    const closing = server.close();
    server.httpServer?.closeAllConnections?.();
    await closing;
  } catch (error) {
    process.stderr.write(
      `companion: closing the ${label} Vite server failed: ${error.message}\n`,
    );
  }
}

function delay(milliseconds) {
  return new Promise((resolve) => {
    setTimeout(resolve, milliseconds);
  });
}

async function sync(params) {
  const framework = requireFramework(params.framework);
  const files = normalizeFiles(params.files);
  const revision = requireRevision(params.revision);
  if (previewState !== null && framework !== previewState.framework) {
    throw coded('invalid_request', 'sync framework does not match the running preview');
  }
  const api = await sourceApi();
  let mapped;
  try {
    mapped = mapDrafts(api, framework, files, revision);
  } catch (error) {
    throw coded(error.code ?? 'parse_failed', error.message);
  }
  if (previewState === null) {
    // The editor always starts the preview before its first operation. A sync
    // in any other order is still a pure validation and has nothing to serve.
    return { ok: true };
  }
  // The draft plugin holds the same Map objects that were created at start.
  // Replace their contents instead of swapping the objects, or the running
  // preview would keep serving the previous revision after a full reload.
  previewState.drafts.clear();
  for (const [key, value] of mapped.drafts) previewState.drafts.set(key, value);
  previewState.overlays.clear();
  for (const [key, value] of mapped.overlays) previewState.overlays.set(key, value);
  previewState.revision = revision;
  previewState.editServer.moduleGraph.invalidateAll();
  return { ok: true };
}

// ---------------------------------------------------------------- line service

let shuttingDown = false;
let shutdownPromise = null;

// A dependency optimization that is still in flight can deadlock Vite's own
// close. The companion must still answer the shutdown request and exit, so the
// wait is bounded; the process exit then tears down whatever Vite left open.
const CLOSE_GRACE_MS = 3000;

async function closeServers() {
  if (previewState === null) return;
  const state = previewState;
  previewState = null;
  const closed = Promise.allSettled([
    closeQuietly(state.editServer, 'edit'),
    closeQuietly(state.normalServer, 'normal'),
  ]);
  const settled = await Promise.race([
    closed.then(() => true),
    delay(CLOSE_GRACE_MS).then(() => false),
  ]);
  if (!settled) {
    process.stderr.write(
      `companion: Vite close did not settle within ${CLOSE_GRACE_MS}ms; exiting\n`,
    );
  }
}

async function shutdown() {
  if (shutdownPromise !== null) return shutdownPromise;
  shuttingDown = true;
  shutdownPromise = closeServers();
  return shutdownPromise;
}

const methods = {
  scan,
  parse,
  edit,
  'preview.start': startPreview,
  sync,
  async shutdown() {
    await shutdown();
    return {};
  },
};

function responseFor(id, result) {
  return { id, ok: true, result };
}

function errorFor(id, error) {
  const code = error instanceof CompanionError ? error.code : 'internal';
  const message = error instanceof Error ? error.message : String(error);
  return { id, ok: false, error: { code, message } };
}

function writeLine(value) {
  return new Promise((resolve, reject) => {
    let line;
    try {
      line = `${JSON.stringify(value)}\n`;
    } catch (error) {
      reject(error);
      return;
    }
    if (Buffer.byteLength(line, 'utf8') > MAX_LINE_BYTES) {
      const fallback = `${JSON.stringify(
        errorFor(value.id ?? null, coded('response_too_large', 'response exceeds the 32 MiB line limit')),
      )}\n`;
      process.stdout.write(fallback, (error) => (error ? reject(error) : resolve()));
      return;
    }
    process.stdout.write(line, (error) => (error ? reject(error) : resolve()));
  });
}

async function handleLine(line) {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    throw coded('protocol_error', 'request is not one JSON object');
  }
  if (
    request === null ||
    typeof request !== 'object' ||
    !Number.isInteger(request.id) ||
    typeof request.method !== 'string' ||
    (request.params !== undefined &&
      (request.params === null || typeof request.params !== 'object' || Array.isArray(request.params)))
  ) {
    throw coded('protocol_error', 'request must be {id:integer,method:string,params:object}');
  }
  const { id, method } = request;
  const params = request.params ?? {};
  let response;
  try {
    const handler = methods[method];
    if (handler === undefined) {
      response = errorFor(id, coded('unknown_method', `unknown method ${method}`));
    } else if (shuttingDown && method !== 'shutdown') {
      response = errorFor(id, coded('companion_unavailable', 'the companion is shutting down'));
    } else {
      response = responseFor(id, await handler(params));
    }
  } catch (error) {
    response = errorFor(id, error);
  }
  await writeLine(response);
  if (method === 'shutdown' && response.ok) {
    await shutdown();
    // Give the write callback a chance to flush before exiting.
    process.exit(0);
  }
}

let queue = Promise.resolve();
let pending = [];
let pendingBytes = 0;
let fatal = false;

function failFatal(error) {
  if (fatal) return;
  fatal = true;
  process.stderr.write(`companion: ${error instanceof Error ? error.message : String(error)}\n`);
  // Malformed framing cannot be answered without inventing an id. Terminate so
  // the Rust host observes EOF and reports the companion as unavailable.
  process.exit(2);
}

function onData(chunk) {
  let start = 0;
  while (!fatal) {
    const newline = chunk.indexOf(0x0a, start);
    if (newline === -1) {
      const rest = chunk.subarray(start);
      pending.push(rest);
      pendingBytes += rest.length;
      if (pendingBytes > MAX_LINE_BYTES) {
        failFatal(coded('request_too_large', 'request exceeds the 32 MiB line limit'));
      }
      return;
    }
    const piece = chunk.subarray(start, newline);
    pending.push(piece);
    pendingBytes += piece.length;
    if (pendingBytes > MAX_LINE_BYTES) {
      failFatal(coded('request_too_large', 'request exceeds the 32 MiB line limit'));
      return;
    }
    const line = Buffer.concat(pending, pendingBytes);
    pending = [];
    pendingBytes = 0;
    start = newline + 1;
    if (line.length === 0) continue;
    queue = queue.then(() => handleLine(line)).catch(failFatal);
  }
}

process.stdin.on('data', onData);
process.stdin.on('end', () => {
  // Drain the requests that already arrived before closing. The writer may
  // close stdin immediately after its last line, so exiting here without the
  // queue would drop valid work.
  queue = queue
    .then(() => {
      shuttingDown = true;
      return shutdown();
    })
    .catch(failFatal)
    .finally(() => process.exit(0));
});
process.stdout.on('error', failFatal);
process.on('SIGTERM', () => {
  queue = queue
    .then(() => {
      shuttingDown = true;
      return shutdown();
    })
    .catch(failFatal)
    .finally(() => process.exit(0));
});
process.on('SIGINT', () => {
  queue = queue
    .then(() => {
      shuttingDown = true;
      return shutdown();
    })
    .catch(failFatal)
    .finally(() => process.exit(0));
});
process.on('uncaughtException', failFatal);
process.on('unhandledRejection', failFatal);

process.stderr.write(`companion: ready (pid ${process.pid})\n`);
