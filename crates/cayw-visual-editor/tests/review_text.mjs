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
    `globalThis.__test = { normalizeSelection, contentGroup, sanitizeOperation,
      commitAttribute(name, value) {
        state.editing = true;
        state.selectedId = 'node1';
        let applied;
        applyOperation = operation => { applied = sanitizeOperation(operation); };
        commitInput({ dataset: { commit: 'attribute', name }, value });
        return applied;
      },
      setNodes(nodes) { state.nodes = nodes; },
      selectionActions(selection) {
        state.selectedId = 'container'; state.selection = selection;
        state.editing = true; state.blocked = false;
        let clicked, opened = false, applied = false;
        els.selectionAdd = { addEventListener(_, fn) { clicked = fn; } };
        els.selectionLabel = { getBoundingClientRect() { return {}; } };
        const button = { addEventListener() {} };
        els.selectionParent = button; els.selectionDuplicate = button; els.selectionDelete = button;
        openPopover = () => { opened = true; };
        effectivePlacement = () => 'inside'; toast = () => {};
        applyOperation = () => { applied = true; };
        bindSelectionLabel(); clicked();
        commitInput({ dataset: {commit:'text'}, value:'replacement' });
        return {opened, applied};
      }
    }; })();`
  );

  vm.createContext(sandbox);
  vm.runInContext(instrumented, sandbox);
  return sandbox.__test;
}



for (const length of [4504, 24004]) {
 test(`complete source text survives editor replacement (${length})`,()=>{
  const {sandbox,makeEl}=setupBridgeSandbox({});
  const el=makeEl(); el.innerText='summary only';
  const msg=sandbox.__test.describe({element:el,id:'node1'},'cayw:select');
  const text='  \n'+ 'A'.repeat(length) + '\n TAIL  ';
  const editor=setupEditorSandbox();
  editor.setNodes([{id:'node1',editable:true,text_source:text}]);
  const sel=editor.normalizeSelection(msg);
  assert.equal(sel.text,text);
  assert.equal(sel.textEditable,true);
  assert(editor.contentGroup(sel,'sidebar',true).includes('>'+text+'</textarea>'));
  assert.equal(editor.sanitizeOperation({kind:'text',id:sel.id,text:sel.text}).text,text);
 });
}
test('unmapped summary is not an editable full value',()=>{
 const {sandbox,makeEl}=setupBridgeSandbox({});
 const msg=sandbox.__test.describe({element:makeEl(),id:'old-revision'},'cayw:select');
 const editor=setupEditorSandbox();editor.setNodes([{id:'new-revision',text_source:'complete'}]);
 const sel=editor.normalizeSelection(msg);
 assert.equal(sel.textEditable,false);
 assert.match(editor.contentGroup(sel,'sidebar',true), /data-testid="editor-sidebar-text" disabled/);
});

test('editable containers allow insertion independently of plain-text editing',()=>{
 const editor=setupEditorSandbox();
 const actions=editor.selectionActions({partial:false,textEditable:false});
 assert.equal(actions.opened,true);
 assert.equal(actions.applied,false);
});
test('mapped plain text can commit, while partial selections cannot insert',()=>{
 const editor=setupEditorSandbox();
 const plain=editor.selectionActions({partial:false,textEditable:true});
 assert.equal(plain.opened,true); assert.equal(plain.applied,true);
 const partial=editor.selectionActions({partial:true,textEditable:false});
 assert.equal(partial.opened,false); assert.equal(partial.applied,false);
});

test('long image addresses reach the attribute operation intact', () => {
  const editor = setupEditorSandbox();
  const svg = '<svg xmlns="http://www.w3.org/2000/svg">' +
    '<rect width="10" height="10"/>'.repeat(100) + '</svg>';
  const value = 'data:image/svg+xml,' + encodeURIComponent(svg);
  assert.ok(value.length > 4000);
  const operation = editor.commitAttribute('src', value);
  assert.equal(operation.name, 'src');
  assert.equal(operation.value, value);
});
