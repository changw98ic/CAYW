// Run with: node --test crates/cayw-visual-editor/tests/review_layout.mjs
// Verifies ET-09: layout alignment dropdowns bind to real computedStyle (alignItems & justifyContent).

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const bridgeSource = await readFile(new URL('../assets/bridge.js', import.meta.url), 'utf8');
const editorSource = await readFile(new URL('../assets/editor.js', import.meta.url), 'utf8');

function setupBridgeSandbox(computedStyle) {
  const makeEl = () => ({
    addEventListener: () => {},
    getAttribute: () => '',
    setAttribute: () => {},
    appendChild: () => {},
    getBoundingClientRect: () => ({ left: 10, top: 20, width: 200, height: 100 }),
    innerText: 'Flex Container',
    style: {},
    dataset: {},
    tagName: 'DIV'
  });

  const sandbox = {
    window: { parent: { postMessage: () => {} }, addEventListener: () => {} },
    document: {
      currentScript: { src: 'http://127.0.0.1:4200/__cayw_bridge.js?editor_origin=http://127.0.0.1:4200' },
      addEventListener: () => {},
      createElement: makeEl,
      getElementById: () => null,
      querySelector: makeEl,
      head: makeEl(),
      documentElement: makeEl(),
      body: makeEl()
    },
    location: { href: 'http://127.0.0.1:4200/index.html?cayw_edit=1', search: '?cayw_edit=1' },
    getComputedStyle: () => computedStyle,
    URL,
    URLSearchParams,
    MessageChannel: class { port1 = { onmessage: null, postMessage: () => {}, start: () => {} }; port2 = {}; },
    globalThis: {}
  };
  sandbox.globalThis = sandbox;

  const instrumented = bridgeSource.replace(
    /\}\)\(\);\s*$/,
    'globalThis.__test = { describe }; })();'
  );

  vm.createContext(sandbox);
  vm.runInContext(instrumented, sandbox);
  return { sandbox, makeEl };
}

function setupEditorSandbox() {
  const makeEl = () => ({
    addEventListener: () => {},
    appendChild: () => {},
    classList: { add: () => {}, remove: () => {}, contains: () => false },
    dataset: {},
    style: {}
  });

  const sandbox = {
    window: { addEventListener: () => {} },
    document: {
      addEventListener: () => {},
      createElement: makeEl,
      getElementById: makeEl,
      querySelector: makeEl,
      querySelectorAll: () => [],
      body: makeEl()
    },
    location: { href: 'http://127.0.0.1:4200/' },
    globalThis: {}
  };
  sandbox.globalThis = sandbox;

  const instrumented = editorSource.replace(
    /\}\)\(\);\s*$/,
    'globalThis.__test = { normalizeSelection, layoutGroup }; })();'
  );

  vm.createContext(sandbox);
  vm.runInContext(instrumented, sandbox);
  return sandbox.__test;
}

test('bridge.js extracts alignItems and justifyContent from getComputedStyle', () => {
  const mockStyle = {
    display: 'flex',
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    color: 'rgb(30, 41, 59)'
  };

  const { sandbox, makeEl } = setupBridgeSandbox(mockStyle);
  const element = makeEl();
  const described = sandbox.__test.describe({ element, id: 'node-flex-1' }, 'element');

  assert.equal(described.computed.alignItems, 'center');
  assert.equal(described.computed.justifyContent, 'space-between');
  assert.equal(described.computed.display, 'flex');
});

test('editor.js normalizeSelection preserves alignItems and justifyContent', () => {
  const editor = setupEditorSandbox();

  const selectionData = {
    id: 'node-flex-1',
    tag: 'div',
    rect: { x: 10, y: 20, width: 200, height: 100 },
    computed: {
      display: 'flex',
      flexDirection: 'row',
      alignItems: 'flex-end',
      justifyContent: 'center'
    }
  };

  const normalized = editor.normalizeSelection(selectionData);
  assert.equal(normalized.computed.alignItems, 'flex-end');
  assert.equal(normalized.computed.justifyContent, 'center');
});

test('editor.js layoutGroup selects real computedStyle values instead of falling back to inherit', () => {
  const editor = setupEditorSandbox();

  const sel = editor.normalizeSelection({
    id: 'node-flex-1',
    tag: 'div',
    rect: { x: 10, y: 20, width: 200, height: 100 },
    computed: {
      display: 'flex',
      flexDirection: 'row',
      alignItems: 'center',
      justifyContent: 'space-between'
    }
  });

  const html = editor.layoutGroup(sel, 'desktop', '');

  // alignItems: center should be selected
  assert.match(html, /<option value="center" selected>center<\/option>/);
  assert.doesNotMatch(html, /<option value="inherit" selected>继承<\/option>[\s\S]*?data-prop="align-items"/);

  // justifyContent: space-between should be selected
  assert.match(html, /<option value="space-between" selected>space-between<\/option>/);
  assert.doesNotMatch(html, /<option value="inherit" selected>继承<\/option>[\s\S]*?data-prop="justify-content"/);
});

test('editor.js layoutGroup explicitly selects inherit when computed alignItems/justifyContent is inherit', () => {
  const editor = setupEditorSandbox();

  const sel = editor.normalizeSelection({
    id: 'node-block-1',
    tag: 'div',
    rect: { x: 10, y: 20, width: 200, height: 100 },
    computed: {
      display: 'block',
      alignItems: 'inherit',
      justifyContent: 'inherit'
    }
  });

  const html = editor.layoutGroup(sel, 'desktop', '');

  const alignSection = html.slice(html.indexOf('data-prop="align-items"'));
  assert.match(alignSection, /<option value="inherit" selected>继承<\/option>/);

  const justifySection = html.slice(html.indexOf('data-prop="justify-content"'));
  assert.match(justifySection, /<option value="inherit" selected>继承<\/option>/);
});

test('editor.js layoutGroup has no non-inherit selected option when computed alignItems is unset', () => {
  const editor = setupEditorSandbox();

  const sel = editor.normalizeSelection({
    id: 'node-block-2',
    tag: 'div',
    rect: { x: 10, y: 20, width: 200, height: 100 },
    computed: {
      display: 'block',
      alignItems: '',
      justifyContent: ''
    }
  });

  const html = editor.layoutGroup(sel, 'desktop', '');

  const alignSection = html.slice(html.indexOf('data-prop="align-items"'), html.indexOf('data-prop="justify-content"'));
  assert.doesNotMatch(alignSection, /value="(stretch|flex-start|center|flex-end|baseline)" selected/);

  const justifySection = html.slice(html.indexOf('data-prop="justify-content"'));
  assert.doesNotMatch(justifySection, /value="(flex-start|center|flex-end|space-between|space-around|space-evenly)" selected/);
});

test('end-to-end: bridge describe into normalizeSelection into layoutGroup reflects styled flex container', () => {
  const mockStyle = {
    display: 'flex',
    flexDirection: 'row',
    alignItems: 'stretch',
    justifyContent: 'space-around',
    color: '#000'
  };

  const { sandbox, makeEl } = setupBridgeSandbox(mockStyle);
  const described = sandbox.__test.describe({ element: makeEl(), id: 'container-1' }, 'element');

  const editor = setupEditorSandbox();
  const normalized = editor.normalizeSelection(described);
  const html = editor.layoutGroup(normalized, 'desktop', '');

  assert.match(html, /<option value="stretch" selected>stretch<\/option>/);
  assert.match(html, /<option value="space-around" selected>space-around<\/option>/);
});
