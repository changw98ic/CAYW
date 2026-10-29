import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { parse } from '@babel/parser';
import { mapDocuments, editDocuments } from './source.mjs';

const REVISION = 'a'.repeat(64);

function applyPatches(files, patches) {
  const buffers = new Map(files.map((file) => [file.path, Buffer.from(file.source, 'utf8')]));
  const ordered = [...patches].sort((left, right) => right.start - left.start);
  for (const patch of ordered) {
    const buffer = buffers.get(patch.path);
    assert.ok(buffer, `unknown patch path ${patch.path}`);
    assert.equal(
      buffer.subarray(patch.start, patch.end).toString('utf8'),
      patch.before,
      `patch before text must match ${patch.path}`,
    );
    buffers.set(patch.path, Buffer.concat([
      buffer.subarray(0, patch.start),
      Buffer.from(patch.replacement, 'utf8'),
      buffer.subarray(patch.end),
    ]));
  }
  return new Map([...buffers].map(([path, buffer]) => [path, buffer.toString('utf8')]));
}

function nodeByTag(nodes, tag, file) {
  const node = nodes.find((candidate) => candidate.tag === tag && (!file || candidate.file === file));
  assert.ok(node, `missing node <${tag}>`);
  return node;
}

function findStyleAttribute(node) {
  if (!node || typeof node !== 'object') return null;
  if (Array.isArray(node)) {
    for (const item of node) {
      const found = findStyleAttribute(item);
      if (found) return found;
    }
    return null;
  }
  if (node.type === 'JSXAttribute' && node.name && node.name.name === 'style') return node;
  for (const key of Object.keys(node)) {
    if (key === 'loc' || key === 'extra') continue;
    const found = findStyleAttribute(node[key]);
    if (found) return found;
  }
  return null;
}

// React only accepts a style object. A JSX string attribute reparses but crashes
// at runtime, so extract the emitted expression and evaluate its real value.
function reactStyleObject(source) {
  const ast = parse(source, { sourceType: 'module', plugins: ['jsx', 'typescript'] });
  const attribute = findStyleAttribute(ast);
  assert.ok(attribute, 'the generated source has no style attribute');
  assert.equal(attribute.value.type, 'JSXExpressionContainer');
  assert.equal(attribute.value.expression.type, 'ObjectExpression');
  const expression = source.slice(attribute.value.expression.start, attribute.value.expression.end);
  const value = new Function(`"use strict"; return (${expression});`)();
  assert.ok(value && typeof value === 'object' && !Array.isArray(value));
  return value;
}

function appendInserted(files, operation, componentHtml, framework, revision) {
  const result = editDocuments({
    framework,
    files,
    revision,
    operation,
    componentHtml,
  });
  return { result, output: applyPatches(files, result.patches) };
}

const REACT_PAGE = `import { useState } from 'react';

export function App() {
  const [count, setCount] = useState(0);
  return (
    <main className="page">
      <h1 title="heading">Hello 🌍</h1>
      <button onClick={() => setCount(count + 1)}>Count</button>
      <p>Static copy</p>
    </main>
  );
}
`;

const VUE_PAGE = `<template>
  <main class="page">
    <h1 :title="label">{{ label }} 🌍</h1>
    <p>Hello</p>
    <li v-for="item in items">{{ item }}</li>
  </main>
</template>

<script setup>
const label = 'x';
const items = [];
</script>

<style scoped>
h1 { color: red; }
</style>
`;

test('react mapping yields deterministic opaque ids and injects only native elements', () => {
  const files = [{ path: 'src/App.jsx', source: REACT_PAGE }];
  const first = mapDocuments({ framework: 'react', files, revision: REVISION });
  const second = mapDocuments({ framework: 'react', files, revision: REVISION });
  assert.deepEqual(first.nodes, second.nodes);
  assert.deepEqual(first.nodes.map((node) => node.tag), ['main', 'h1', 'button', 'p']);
  for (const node of first.nodes) {
    assert.match(node.id, /^n[0-9a-f]+-[0-9a-f]{64}$/);
    assert.ok(Buffer.byteLength(node.id, 'utf8') <= 128);
    assert.equal(node.file, 'src/App.jsx');
    assert.equal(node.editable, true);
  }
  assert.equal(first.nodes[1].parent, first.nodes[0].id);
  assert.equal(first.nodes[3].parent, first.nodes[0].id);
  const instrumented = first.instrumented['src/App.jsx'];
  assert.match(instrumented, /<main data-cayw-node="n0-/);
  assert.match(instrumented, /<h1 data-cayw-node="n1-/);
  assert.equal((instrumented.match(/data-cayw-node/g) || []).length, 4);
});

test('upper-case components are not mapped and their native children have no mapped parent', () => {
  const source = `export const A = () => (<Card title="x"><div><span>Hi</span></div></Card>);`;
  const { nodes, instrumented } = mapDocuments({
    framework: 'react',
    files: [{ path: 'A.jsx', source }],
    revision: REVISION,
  });
  assert.deepEqual(nodes.map((node) => node.tag), ['div', 'span']);
  assert.equal(nodes[0].parent, null);
  assert.equal(nodes[1].parent, nodes[0].id);
  assert.doesNotMatch(instrumented['A.jsx'], /<Card data-cayw/);
  assert.match(instrumented['A.jsx'], /<div data-cayw-node/);
});

test('utf8 byte offsets and lines stay correct with emoji and CRLF', () => {
  const source = `const A = () => (\r\n  <div>\r\n    <p>🌍</p>\r\n    <span title="t">Hi</span>\r\n  </div>\r\n);\r\n`;
  const files = [{ path: 'A.jsx', source }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const div = nodeByTag(nodes, 'div');
  const paragraph = nodeByTag(nodes, 'p');
  const span = nodeByTag(nodes, 'span');
  assert.equal(div.start, source.indexOf('<div>'));
  assert.equal(paragraph.start, Buffer.byteLength(source.slice(0, source.indexOf('<p>')), 'utf8'));
  assert.equal(span.start, Buffer.byteLength(source.slice(0, source.indexOf('<span')), 'utf8'));
  assert.equal(div.line, 2);
  assert.equal(paragraph.line, 3);
  assert.equal(span.line, 4);

  const result = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'text', id: span.id, text: '你好' },
  });
  assert.equal(result.patches[0].before, 'Hi');
  const output = applyPatches(files, result.patches).get('A.jsx');
  assert.match(output, /<span title="t">你好<\/span>/);
  assert.match(output, /<p>🌍<\/p>/);
  assert.equal(result.selection.start, span.start);
});

test('tsx files parse type syntax and map native elements', () => {
  const source = `interface Props { label: string }\nexport const A = ({ label }: Props) => (<div><span>{label}</span></div>);`;
  const { nodes } = mapDocuments({
    framework: 'react',
    files: [{ path: 'A.tsx', source }],
    revision: REVISION,
  });
  assert.deepEqual(nodes.map((node) => node.tag), ['div', 'span']);
  const span = nodeByTag(nodes, 'span');
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files: [{ path: 'A.tsx', source }],
      revision: REVISION,
      operation: { kind: 'text', id: span.id, text: 'x' },
    }),
    (error) => error.code === 'dynamic_region',
  );
});

test('mts and cts modules parse as TypeScript without blocking edits in other files', () => {
  const files = [
    { path: 'src/App.jsx', source: `<div><p>Hi</p></div>` },
    { path: 'src/helper.mts', source: `export interface Item { id: string }\nexport const make = (id: string): Item => ({ id });\n` },
    { path: 'src/legacy.cts', source: `type Mode = 'a' | 'b';\nexport const mode: Mode = 'a';\n` },
  ];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  assert.deepEqual(nodes.map((node) => node.tag), ['div', 'p']);
  const result = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'text', id: nodeByTag(nodes, 'p').id, text: 'Hello' },
  });
  const output = applyPatches(files, result.patches).get('src/App.jsx');
  assert.match(output, /<p>Hello<\/p>/);
});

test('vue mapping keeps script and scoped style byte-for-byte and maps the template', () => {
  const files = [{ path: 'src/App.vue', source: VUE_PAGE }];
  const { nodes, instrumented } = mapDocuments({
    framework: 'vue',
    files,
    revision: REVISION,
  });
  assert.deepEqual(nodes.map((node) => node.tag), ['main', 'h1', 'p', 'li']);
  assert.equal(nodeByTag(nodes, 'main').parent, null);
  assert.equal(nodeByTag(nodes, 'h1').parent, nodeByTag(nodes, 'main').id);
  assert.equal(nodeByTag(nodes, 'li').editable, false);
  assert.equal(nodeByTag(nodes, 'li').reason, 'dynamic_region');

  const output = instrumented['src/App.vue'];
  assert.match(output, /<main data-cayw-node=/);
  assert.match(output, /<h1 data-cayw-node=.* :title="label">/);
  assert.match(output, /<li data-cayw-node=.* data-cayw-readonly="true"/);
  assert.ok(output.endsWith(VUE_PAGE.slice(VUE_PAGE.indexOf('</template>') + '</template>'.length)));
  assert.match(output, /<script setup>\nconst label = 'x';\nconst items = \[\];\n<\/script>/);
  assert.match(output, /<style scoped>\nh1 \{ color: red; \}\n<\/style>/);
  assert.equal(nodeByTag(nodes, 'h1').line, 3);
});

test('style and text edits keep handlers and custom property case', () => {
  const files = [{ path: 'src/App.jsx', source: REACT_PAGE }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const button = nodeByTag(nodes, 'button');
  const h1 = nodeByTag(nodes, 'h1');

  const styled = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: {
      kind: 'style',
      id: button.id,
      properties: { color: 'red', '--Brand-Color': 'var(--accent)' },
    },
  });
  let output = applyPatches(files, styled.patches).get('src/App.jsx');
  assert.match(output, /style=\{\{ color: "red", "--Brand-Color": "var\(--accent\)" \}\}/);
  assert.doesNotMatch(output, /style="/);
  assert.match(output, /onClick=\{\(\) => setCount\(count \+ 1\)\}/);
  assert.equal(styled.selection.start, button.start);

  const text = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'text', id: h1.id, text: '你好 🌍' },
  });
  output = applyPatches(files, text.patches).get('src/App.jsx');
  assert.match(output, /<h1 title="heading">你好 🌍<\/h1>/);
  assert.equal(text.selection.start, h1.start);
});

test('react style edits always produce executable style objects', () => {
  const files = [{
    path: 'src/App.jsx',
    source: `<main><h1>Hi</h1><p style="margin-top: 4px">Copy</p></main>`,
  }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });

  const added = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: {
      kind: 'style',
      id: nodeByTag(nodes, 'h1').id,
      properties: { color: '#123456', 'font-size': '18px', '--Brand-Color': 'var(--accent)' },
    },
  });
  const withAdded = applyPatches(files, added.patches).get('src/App.jsx');
  assert.match(withAdded, /<h1 style=\{\{/);
  assert.doesNotMatch(withAdded, /<h1[^>]*style="/);
  assert.deepEqual(reactStyleObject(withAdded), {
    color: '#123456',
    fontSize: '18px',
    '--Brand-Color': 'var(--accent)',
  });

  const converted = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'style', id: nodeByTag(nodes, 'p').id, properties: { color: 'blue' } },
  });
  const withConverted = applyPatches(files, converted.patches).get('src/App.jsx');
  assert.match(withConverted, /<p style=\{\{/);
  assert.doesNotMatch(withConverted, /<p[^>]*style="/);
  assert.deepEqual(reactStyleObject(withConverted), { marginTop: '4px', color: 'blue' });
});

test('react style edits preserve numeric AST literals and reject dynamic objects', () => {
  const files = [{
    path: 'src/App.jsx',
    source: `<main><div style={{ width: 10, marginTop: -2, opacity: 0.5 }}>Test</div><span style={dynamicStyle}>Dynamic</span></main>`,
  }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const div = nodeByTag(nodes, 'div');
  const span = nodeByTag(nodes, 'span');

  const styled = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: {
      kind: 'style',
      id: div.id,
      properties: { color: 'blue' },
    },
  });
  const output = applyPatches(files, styled.patches).get('src/App.jsx');
  assert.match(output, /style=\{\{ width: 10, marginTop: -2, opacity: 0.5, color: "blue" \}\}/);
  const styleObj = reactStyleObject(output);
  assert.equal(styleObj.width, 10);
  assert.equal(typeof styleObj.width, 'number');
  assert.equal(styleObj.marginTop, -2);
  assert.equal(typeof styleObj.marginTop, 'number');
  assert.equal(styleObj.opacity, 0.5);
  assert.equal(typeof styleObj.opacity, 'number');
  assert.equal(styleObj.color, 'blue');

  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'style', id: span.id, properties: { color: 'red' } },
    }),
    (error) => error.code === 'dynamic_region',
    'dynamic style objects must be refused',
  );
});

test('tsx style edits use the same object form', () => {
  const files = [{
    path: 'src/A.tsx',
    source: `export function A(): JSX.Element {\n  return <section><strong>Hi</strong></section>;\n}\n`,
  }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const result = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'style', id: nodeByTag(nodes, 'strong').id, properties: { color: 'green' } },
  });
  const output = applyPatches(files, result.patches).get('src/A.tsx');
  assert.doesNotMatch(output, /style="/);
  assert.deepEqual(reactStyleObject(output), { color: 'green' });
  assert.match(output, /<\/section>;/);
});

function canonicalComponents() {
  const rust = readFileSync(new URL('../src/project.rs', import.meta.url), 'utf8');
  const found = [];
  const pattern = /const (COMPONENT_[A-Z]+): &str = "((?:[^"\\]|\\.)*)";/g;
  let match = pattern.exec(rust);
  while (match) {
    const html = match[2]
      .replace(/\\(.)/g, (_, character) => (character === 'n' ? '\n' : character === 't' ? '\t' : character))
      .replaceAll('DATA_IMAGE', 'data:image/png;base64,AA');
    found.push({ name: match[1], html });
    match = pattern.exec(rust);
  }
  assert.equal(found.length, 11, 'expected the 11 canonical components in project.rs');
  return found;
}

function componentCount(source, component) {
  return (source.match(new RegExp(`data-cayw-component="${component}"`, 'g')) || []).length;
}

function rootNode(nodes, result) {
  const root = nodes.find((node) => node.start === result.selection.start);
  assert.ok(root, 'the inserted root was not mapped');
  return root;
}

test('all canonical React snippets support style, duplicate, delete and move', () => {
  for (const canonical of canonicalComponents()) {
    const component = /data-cayw-component="([^"]+)"/.exec(canonical.html)[1];
    const files = [{ path: 'A.jsx', source: `<main><div><span>x</span></div></main>` }];
    const div = nodeByTag(
      mapDocuments({ framework: 'react', files, revision: REVISION }).nodes,
      'div',
    );
    const inserted = editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'insert', target: div.id, placement: 'inside', component },
      componentHtml: canonical.html,
    });
    const source = applyPatches(files, inserted.patches).get('A.jsx');
    assert.equal(componentCount(source, component), 1, `${canonical.name}: insert count`);
    const insertedFiles = [{ path: 'A.jsx', source }];
    const nodes = mapDocuments({ framework: 'react', files: insertedFiles, revision: REVISION }).nodes;
    const root = rootNode(nodes, inserted);

    const beforeStyle = reactStyleObject(source);
    const styled = editDocuments({
      framework: 'react',
      files: insertedFiles,
      revision: REVISION,
      operation: { kind: 'style', id: root.id, properties: { color: '#123456' } },
    });
    const styledSource = applyPatches(insertedFiles, styled.patches).get('A.jsx');
    const afterStyle = reactStyleObject(styledSource);
    for (const [key, value] of Object.entries(beforeStyle)) {
      if (key === 'color') continue;
      assert.equal(afterStyle[key], value, `${canonical.name}: style merge kept ${key}`);
    }
    assert.equal(afterStyle.color, '#123456', `${canonical.name}: style applied`);
    assert.doesNotMatch(styledSource, /style="/);

    const duplicate = editDocuments({
      framework: 'react',
      files: insertedFiles,
      revision: REVISION,
      operation: { kind: 'duplicate', id: root.id },
    });
    const duplicateSource = applyPatches(insertedFiles, duplicate.patches).get('A.jsx');
    assert.equal(componentCount(duplicateSource, component), 2, `${canonical.name}: duplicate`);

    const removed = editDocuments({
      framework: 'react',
      files: insertedFiles,
      revision: REVISION,
      operation: { kind: 'delete', id: root.id },
    });
    const removedSource = applyPatches(insertedFiles, removed.patches).get('A.jsx');
    assert.equal(componentCount(removedSource, component), 0, `${canonical.name}: delete`);

    const moved = editDocuments({
      framework: 'react',
      files: insertedFiles,
      revision: REVISION,
      operation: { kind: 'move', id: root.id, target: div.id, placement: 'before' },
    });
    const movedSource = applyPatches(insertedFiles, moved.patches).get('A.jsx');
    assert.equal(componentCount(movedSource, component), 1, `${canonical.name}: move count`);
    const movedNodes = mapDocuments({
      framework: 'react',
      files: [{ path: 'A.jsx', source: movedSource }],
      revision: REVISION,
    }).nodes;
    const movedRoot = movedNodes.find((node) => node.start === moved.selection.start);
    assert.ok(movedRoot, `${canonical.name}: moved root mapped`);
    assert.equal(
      movedRoot.parent,
      nodeByTag(movedNodes, 'main').id,
      `${canonical.name}: moved root is now a sibling of the insert container`,
    );
  }
});

test('computed style values and expressions stay structurally protected', () => {
  const files = [{
    path: 'A.jsx',
    source: `<div><button style={{ color: "#123456", padding: "4px" }}>x</button><p style={{ color: accent }}>y</p><em style={{ ...base, color: "red" }}>z</em><span>{count}</span></div>`,
  }];
  const nodes = mapDocuments({ framework: 'react', files, revision: REVISION }).nodes;
  for (const tag of ['p', 'em', 'span']) {
    assert.throws(
      () => editDocuments({
        framework: 'react',
        files,
        revision: REVISION,
        operation: { kind: 'duplicate', id: nodeByTag(nodes, tag).id },
      }),
      (error) => error.code === 'dynamic_region',
      `${tag} must stay protected`,
    );
  }
  const button = nodeByTag(nodes, 'button');
  const duplicated = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'duplicate', id: button.id },
  });
  const output = applyPatches(files, duplicated.patches).get('A.jsx');
  assert.equal((output.match(/style=\{\{ color: "#123456", padding: "4px" \}\}/g) || []).length, 2);
});

test('attribute edits update static attributes and reject dynamic targets', () => {
  const files = [{ path: 'A.jsx', source: `<div className="a" title="t"><span>Hi</span></div>` }];
  // `class` is not in the editable attribute set; use `title` instead.
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const div = nodeByTag(nodes, 'div');
  const result = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'attribute', id: div.id, name: 'title', value: 'new' },
  });
  const output = applyPatches(files, result.patches).get('A.jsx');
  assert.match(output, /title="new"/);

  const dynamic = [{ path: 'A.jsx', source: `<div title={label}>Hi</div>` }];
  const dynamicNodes = mapDocuments({
    framework: 'react',
    files: dynamic,
    revision: REVISION,
  }).nodes;
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files: dynamic,
      revision: REVISION,
      operation: { kind: 'attribute', id: nodeByTag(dynamicNodes, 'div').id, name: 'title', value: 'x' },
    }),
    (error) => error.code === 'dynamic_region',
  );
});

test('dynamic style and text operations are refused on the affected target', () => {
  const files = [{
    path: 'A.jsx',
    source: `<div><p style={{ color: value }}>x</p><span>{value}</span></div>`,
  }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const paragraph = nodeByTag(nodes, 'p');
  const span = nodeByTag(nodes, 'span');
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'style', id: paragraph.id, properties: { color: 'blue' } },
    }),
    (error) => error.code === 'dynamic_region',
  );
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'text', id: span.id, text: 'static' },
    }),
    (error) => error.code === 'dynamic_region',
  );
});

test('spread, conditional and repeated regions are read-only', () => {
  const files = [{
    path: 'A.jsx',
    source: `<div><section {...props}>x</section>{ok && <p>yes</p>}{items.map((item) => <span key={item}>{item}</span>)}</div>`,
  }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const section = nodeByTag(nodes, 'section');
  const paragraph = nodeByTag(nodes, 'p');
  const span = nodeByTag(nodes, 'span');
  assert.equal(section.editable, false);
  assert.equal(paragraph.editable, false);
  assert.equal(span.editable, false);
  assert.equal(section.reason, 'dynamic_region');
});

test('a static parent with dynamic children still accepts an inside insert', () => {
  const files = [{
    path: 'A.jsx',
    source: `<div><span>{value}</span></div>`,
  }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const div = nodeByTag(nodes, 'div');
  assert.equal(div.editable, true);
  const { result } = appendInserted(
    files,
    { kind: 'insert', target: div.id, placement: 'inside', component: 'card' },
    `<section><p>New</p></section>`,
    'react',
    REVISION,
  );
  assert.equal(result.patches.length, 1);
});

test('jsx insert converts class, self-closes void tags and keeps style blocks valid', () => {
  const files = [{ path: 'A.jsx', source: `<div><span>x</span></div>` }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const div = nodeByTag(nodes, 'div');
  const component = `<style data-cayw-component-css="motion">@keyframes fade{from{opacity:0}to{opacity:1}}</style><section class="panel" style="display:grid"><img src="data:image/png;base64,AA" alt="x"><p>Hello</p></section>`;
  const { result, output } = appendInserted(
    files,
    { kind: 'insert', target: div.id, placement: 'inside', component: 'hero' },
    component,
    'react',
    REVISION,
  );
  const inserted = output.get('A.jsx');
  assert.match(inserted, /className="panel"/);
  assert.match(inserted, /style=\{\{ display: "grid" \}\}/);
  assert.doesNotMatch(inserted, /style="/);
  assert.match(inserted, /<img src="data:image\/png;base64,AA" alt="x" \/>/);
  assert.match(inserted, /<style data-cayw-component-css="motion">\{`@keyframes fade/);
  assert.equal(result.selection.path, 'A.jsx');
  // The diagnostics expose the inserted <section>, not the non-editable style.
  const reparsed = mapDocuments({
    framework: 'react',
    files: [{ path: 'A.jsx', source: inserted }],
    revision: 'b'.repeat(64),
  }).nodes;
  assert.equal(nodeByTag(reparsed, 'section').start, result.selection.start);
});

test('insert rejects unsafe component html', () => {
  const files = [{ path: 'A.jsx', source: `<div><span>x</span></div>` }];
  const div = nodeByTag(
    mapDocuments({ framework: 'react', files, revision: REVISION }).nodes,
    'div',
  );
  for (const componentHtml of [
    `<script>alert(1)</script>`,
    `<link href="x">`,
    `<div onclick="x">a</div>`,
    `<div style="background:url(javascript:1)">a</div>`,
    `<style>@import 'x';</style><div>a</div>`,
  ]) {
    assert.throws(
      () => editDocuments({
        framework: 'react',
        files,
        revision: REVISION,
        operation: { kind: 'insert', target: div.id, placement: 'inside', component: 'card' },
        componentHtml,
      }),
      (error) => error.code === 'invalid_operation',
    );
  }
});

test('all react structural operations apply and reparse', () => {
  const files = [{ path: 'A.jsx', source: `<div><h1>Title</h1><p>one</p><span>two</span></div>` }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const div = nodeByTag(nodes, 'div');
  const h1 = nodeByTag(nodes, 'h1');
  const paragraph = nodeByTag(nodes, 'p');
  const span = nodeByTag(nodes, 'span');

  const duplicate = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'duplicate', id: paragraph.id },
  });
  let output = applyPatches(files, duplicate.patches).get('A.jsx');
  assert.equal((output.match(/<p>one<\/p>/g) || []).length, 2);
  assert.equal(duplicate.selection.start, paragraph.start + Buffer.byteLength('<p>one</p>'));

  const move = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'move', id: h1.id, target: span.id, placement: 'after' },
  });
  output = applyPatches(files, move.patches).get('A.jsx');
  assert.match(output, /<p>one<\/p><span>two<\/span><h1>Title<\/h1>/);
  assert.equal(
    Buffer.from(output, 'utf8').subarray(move.selection.start).toString('utf8').startsWith('<h1>Title</h1>'),
    true,
  );

  const remove = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'delete', id: paragraph.id },
  });
  output = applyPatches(files, remove.patches).get('A.jsx');
  assert.doesNotMatch(output, /<p>one<\/p>/);
  assert.equal(remove.selection, null);

  const inserted = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'insert', target: div.id, placement: 'inside', component: 'text' },
    componentHtml: `<p>new</p>`,
  });
  output = applyPatches(files, inserted.patches).get('A.jsx');
  assert.match(output, /<span>two<\/span><p>new<\/p><\/div>/);
});

test('structural operations refuse handlers, ids and root adjacency', () => {
  const files = [{
    path: 'A.jsx',
    source: `<div><button onClick={() => {}}>x</button><p id="p">y</p></div>`,
  }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const button = nodeByTag(nodes, 'button');
  const paragraph = nodeByTag(nodes, 'p');
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'delete', id: button.id },
    }),
    (error) => error.code === 'dynamic_region',
  );
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'duplicate', id: paragraph.id },
    }),
    (error) => error.code === 'unsupported_target',
  );
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'delete', id: nodeByTag(nodes, 'div').id },
    }),
    (error) => error.code === 'dynamic_region',
  );
});

test('text edits on a handler element keep the handler intact', () => {
  const files = [{ path: 'A.jsx', source: `<div><button onClick={() => go()}>Old</button></div>` }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const button = nodeByTag(nodes, 'button');
  const result = editDocuments({
    framework: 'react',
    files,
    revision: REVISION,
    operation: { kind: 'text', id: button.id, text: 'New' },
  });
  const output = applyPatches(files, result.patches).get('A.jsx');
  assert.match(output, /<button onClick=\{\(\) => go\(\)\}>New<\/button>/);
});

test('structural edits refuse a subtree that contains a script', () => {
  const files = [{
    path: 'A.jsx',
    source: `<div><script dangerouslySetInnerHTML={{ __html: 'x' }} /><p>y</p></div>`,
  }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  assert.equal(nodeByTag(nodes, 'script').editable, false);
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'delete', id: nodeByTag(nodes, 'div').id },
    }),
    (error) => error.code === 'unsupported_target' && /script/.test(error.message),
  );
});

test('root-level react duplicate is refused instead of producing adjacent elements', () => {
  const files = [{ path: 'A.jsx', source: `const A = () => (<div><p>x</p></div>);` }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'duplicate', id: nodeByTag(nodes, 'div').id },
    }),
    (error) => error.code === 'unsupported_target',
  );
});

test('cross-file moves are rejected', () => {
  const files = [
    { path: 'A.jsx', source: `<div><p>x</p></div>` },
    { path: 'B.jsx', source: `<section><span>y</span></section>` },
  ];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: {
        kind: 'move',
        id: nodeByTag(nodes, 'p').id,
        target: nodeByTag(nodes, 'span').id,
        placement: 'after',
      },
    }),
    (error) => error.code === 'cross_file',
  );
});

test('stale and malformed ids are rejected with distinct codes', () => {
  const files = [{ path: 'A.jsx', source: `<div><p>x</p></div>` }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  const paragraph = nodeByTag(nodes, 'p');
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: 'c'.repeat(64),
      operation: { kind: 'text', id: paragraph.id, text: 'x' },
    }),
    (error) => error.code === 'stale_id',
  );
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'text', id: 'nope', text: 'x' },
    }),
    (error) => error.code === 'not_found',
  );
});

test('vue attribute, text, style and structural operations respect directives', () => {
  const files = [{ path: 'src/App.vue', source: VUE_PAGE }];
  const { nodes } = mapDocuments({ framework: 'vue', files, revision: REVISION });
  const main = nodeByTag(nodes, 'main');
  const h1 = nodeByTag(nodes, 'h1');
  const paragraph = nodeByTag(nodes, 'p');

  assert.throws(
    () => editDocuments({
      framework: 'vue',
      files,
      revision: REVISION,
      operation: { kind: 'text', id: h1.id, text: 'x' },
    }),
    (error) => error.code === 'dynamic_region',
  );
  assert.throws(
    () => editDocuments({
      framework: 'vue',
      files,
      revision: REVISION,
      operation: { kind: 'attribute', id: h1.id, name: 'title', value: 'x' },
    }),
    (error) => error.code === 'dynamic_region',
  );

  const styled = editDocuments({
    framework: 'vue',
    files,
    revision: REVISION,
    operation: { kind: 'style', id: h1.id, properties: { color: 'blue' } },
  });
  let output = applyPatches(files, styled.patches).get('src/App.vue');
  assert.match(output, /<h1 style="color: blue" :title="label">/);

  const text = editDocuments({
    framework: 'vue',
    files,
    revision: REVISION,
    operation: { kind: 'text', id: paragraph.id, text: '你好' },
  });
  output = applyPatches(files, text.patches).get('src/App.vue');
  assert.match(output, /<p>你好<\/p>/);

  const duplicate = editDocuments({
    framework: 'vue',
    files,
    revision: REVISION,
    operation: { kind: 'duplicate', id: paragraph.id },
  });
  output = applyPatches(files, duplicate.patches).get('src/App.vue');
  assert.equal((output.match(/<p>Hello<\/p>/g) || []).length, 2);

  const moved = editDocuments({
    framework: 'vue',
    files,
    revision: REVISION,
    operation: { kind: 'move', id: paragraph.id, target: h1.id, placement: 'before' },
  });
  output = applyPatches(files, moved.patches).get('src/App.vue');
  assert.ok(output.indexOf('<p>Hello</p>') < output.indexOf('<h1'));

  const removed = editDocuments({
    framework: 'vue',
    files,
    revision: REVISION,
    operation: { kind: 'delete', id: paragraph.id },
  });
  output = applyPatches(files, removed.patches).get('src/App.vue');
  assert.doesNotMatch(output, /<p>Hello<\/p>/);

  const { output: inserted } = appendInserted(
    files,
    { kind: 'insert', target: main.id, placement: 'inside', component: 'card' },
    `<section class="card"><img src="x.png" alt="a"><p>New</p></section>`,
    'vue',
    REVISION,
  );
  assert.match(inserted.get('src/App.vue'), /<section class="card">/);
  assert.match(inserted.get('src/App.vue'), /<\/script>/);
});

test('react style edits reject !important instead of silently dropping it', () => {
  const files = [{
    path: 'A.jsx',
    source: `<div><p>Hi</p><p style="color: red !important">Old</p></div>`,
  }];
  const nodes = mapDocuments({ framework: 'react', files, revision: REVISION }).nodes;
  const [first, second] = nodes.filter((node) => node.tag === 'p');
  for (const [target, properties] of [
    [first, { color: 'blue !important' }],
    [second, { margin: '4px' }],
  ]) {
    assert.throws(
      () => editDocuments({
        framework: 'react',
        files,
        revision: REVISION,
        operation: { kind: 'style', id: target.id, properties },
      }),
      (error) => error.code === 'invalid_style',
      'React cannot represent !important',
    );
  }

  const vueFiles = [{ path: 'A.vue', source: `<template><main><p>Hi</p></main></template>` }];
  const vueParagraph = nodeByTag(
    mapDocuments({ framework: 'vue', files: vueFiles, revision: REVISION }).nodes,
    'p',
  );
  const styled = editDocuments({
    framework: 'vue',
    files: vueFiles,
    revision: REVISION,
    operation: { kind: 'style', id: vueParagraph.id, properties: { color: 'blue !important' } },
  });
  const output = applyPatches(vueFiles, styled.patches).get('A.vue');
  assert.match(output, /<p style="color: blue !important">Hi<\/p>/);
});

test('vue project inserts into a JSX target with the target parser', () => {
  const files = [
    { path: 'src/App.vue', source: `<template><main><div id="host">Vue</div></main></template>` },
    { path: 'src/Widget.jsx', source: `export function Widget() {\n  return <div className="host"><span>Hi</span></div>;\n}\n` },
  ];
  const nodes = mapDocuments({ framework: 'vue', files, revision: REVISION }).nodes;
  const host = nodeByTag(nodes, 'div', 'src/Widget.jsx');
  const hero = canonicalComponents().find((entry) => entry.name === 'COMPONENT_HERO');
  const result = editDocuments({
    framework: 'vue',
    files,
    revision: REVISION,
    operation: { kind: 'insert', target: host.id, placement: 'inside', component: 'hero' },
    componentHtml: hero.html,
  });
  assert.deepEqual(result.patches.map((patch) => patch.path), ['src/Widget.jsx']);
  const output = applyPatches(files, result.patches).get('src/Widget.jsx');
  assert.match(output, /<section[^>]*style=\{\{/);
  assert.match(output, /<style data-cayw-component-css="motion">\{`@keyframes/);
  assert.doesNotMatch(output, /style="/);
  const reparsed = mapDocuments({
    framework: 'vue',
    files: [{ path: 'src/Widget.jsx', source: output }],
    revision: 'c'.repeat(64),
  }).nodes;
  assert.equal(nodeByTag(reparsed, 'section').start, result.selection.start);
});

test('vue insert leaves qualified style blocks untouched and adds a plain block', () => {
  const source = `<template>\n  <main><div id="host"></div></main>\n</template>\n\n<style media="print">\np { color: black; }\n</style>\n\n<style nonce="abc" data-theme="dark">\np { margin: 0; }\n</style>\n`;
  const files = [{ path: 'src/Qualified.vue', source }];
  const host = nodeByTag(
    mapDocuments({ framework: 'vue', files, revision: REVISION }).nodes,
    'div',
  );
  const { result, output } = appendInserted(
    files,
    { kind: 'insert', target: host.id, placement: 'inside', component: 'hero' },
    `<style>@keyframes fade{from{opacity:0}to{opacity:1}}</style><section><p>New</p></section>`,
    'vue',
    REVISION,
  );
  const inserted = output.get('src/Qualified.vue');
  assert.match(inserted, /<style media="print">\np \{ color: black; \}\n<\/style>/);
  assert.match(inserted, /<style nonce="abc" data-theme="dark">\np \{ margin: 0; \}\n<\/style>/);
  const tags = [...inserted.matchAll(/<style([^>]*)>/g)].map((match) => match[1].trim());
  assert.deepEqual(tags, ['media="print"', 'nonce="abc" data-theme="dark"', '']);
  assert.match(inserted, /<style>\n@keyframes fade\{from\{opacity:0\}to\{opacity:1\}\}\n<\/style>/);
  assert.equal(
    mapDocuments({
      framework: 'vue',
      files: [{ path: 'src/Qualified.vue', source: inserted }],
      revision: REVISION,
    }).nodes.find((node) => node.start === result.selection.start).tag,
    'section',
  );
});

test('vue insert moves static component style blocks into the SFC', () => {
  const files = [{ path: 'src/App.vue', source: VUE_PAGE }];
  const main = nodeByTag(
    mapDocuments({ framework: 'vue', files, revision: REVISION }).nodes,
    'main',
  );
  const component = `<style data-cayw-component-css="motion">@keyframes fade{from{opacity:0}to{opacity:1}}</style><section class="panel" style="display:grid"><p>Hello</p></section>`;
  const { result, output } = appendInserted(
    files,
    { kind: 'insert', target: main.id, placement: 'inside', component: 'hero' },
    component,
    'vue',
    REVISION,
  );
  const source = output.get('src/App.vue');
  assert.doesNotMatch(source.slice(0, source.indexOf('</template>')), /<style/);
  assert.match(source, /<section class="panel" style="display:grid"><p>Hello<\/p><\/section>/);
  assert.match(source, /<style scoped>\nh1 \{ color: red; \}\n<\/style>/);
  assert.match(source, /<script setup>\nconst label = 'x';\nconst items = \[\];\n<\/script>/);
  assert.match(source, /<style>\n@keyframes fade\{from\{opacity:0\}to\{opacity:1\}\}\n<\/style>/);
  const reparsed = mapDocuments({
    framework: 'vue',
    files: [{ path: 'src/App.vue', source }],
    revision: 'b'.repeat(64),
  }).nodes;
  assert.equal(reparsed.find((node) => node.start === result.selection.start).tag, 'section');
});

test('vue insert appends to a plain style block before the template and keeps selection', () => {
  const files = [{
    path: 'src/B.vue',
    source: `<style>\np { color: red; }\n</style>\n\n<template>\n  <main><p>Hi</p></main>\n</template>\n`,
  }];
  const main = nodeByTag(
    mapDocuments({ framework: 'vue', files, revision: REVISION }).nodes,
    'main',
  );
  const component = `<style>@keyframes fade{from{opacity:0}to{opacity:1}}</style><section><p>New</p></section>`;
  const { result, output } = appendInserted(
    files,
    { kind: 'insert', target: main.id, placement: 'inside', component: 'hero' },
    component,
    'vue',
    REVISION,
  );
  const source = output.get('src/B.vue');
  assert.match(source, /<style>\np \{ color: red; \}\n@keyframes fade\{from\{opacity:0\}to\{opacity:1\}\}\n<\/style>/);
  assert.doesNotMatch(source.slice(source.indexOf('<template>')), /<style>/);
  const nodes = mapDocuments({
    framework: 'vue',
    files: [{ path: 'src/B.vue', source }],
    revision: REVISION,
  }).nodes;
  assert.equal(nodes.find((node) => node.start === result.selection.start).tag, 'section');
});

test('vue loops and conditional regions are read-only with inherited children', () => {
  const source = `<template><main><ul><li v-for="x in xs"><span>{{ x }}</span></li><p v-if="ok">yes</p></ul></main></template>`;
  const { nodes } = mapDocuments({
    framework: 'vue',
    files: [{ path: 'A.vue', source }],
    revision: REVISION,
  });
  const li = nodeByTag(nodes, 'li');
  const span = nodeByTag(nodes, 'span');
  const paragraph = nodeByTag(nodes, 'p');
  assert.equal(li.editable, false);
  assert.equal(span.editable, false);
  assert.equal(paragraph.editable, false);
  assert.equal(li.reason, 'dynamic_region');
  assert.equal(span.reason, 'dynamic_region');
});

test('vue duplicate with an existing id attribute is refused', () => {
  const source = `<template><main><p id="x">one</p></main></template>`;
  const files = [{ path: 'A.vue', source }];
  const { nodes } = mapDocuments({ framework: 'vue', files, revision: REVISION });
  assert.throws(
    () => editDocuments({
      framework: 'vue',
      files,
      revision: REVISION,
      operation: { kind: 'duplicate', id: nodeByTag(nodes, 'p').id },
    }),
    (error) => error.code === 'unsupported_target',
  );
});

test('setting a duplicate id attribute is rejected', () => {
  const source = `<div id="a"><p id="b">x</p><span id="c">y</span></div>`;
  const files = [{ path: 'A.jsx', source }];
  const { nodes } = mapDocuments({ framework: 'react', files, revision: REVISION });
  assert.throws(
    () => editDocuments({
      framework: 'react',
      files,
      revision: REVISION,
      operation: { kind: 'attribute', id: nodeByTag(nodes, 'span').id, name: 'id', value: 'b' },
    }),
    (error) => error.code === 'invalid_value',
  );
});

test('reserved mapping attributes are removed from the response and stay read-only', () => {
  const source = `<div data-cayw-node="spoof"><p data-cayw-readonly="true">x</p></div>`;
  const result = mapDocuments({
    framework: 'react',
    files: [{ path: 'A.jsx', source }],
    revision: REVISION,
  });
  assert.doesNotMatch(result.instrumented['A.jsx'], /spoof/);
  assert.equal((result.instrumented['A.jsx'].match(/data-cayw-node=/g) || []).length, 1);
  assert.match(result.instrumented['A.jsx'], /<p data-cayw-node="[^"]+" data-cayw-readonly="true" data-cayw-reason="reserved_attribute">/);
  assert.equal(nodeByTag(result.nodes, 'div').editable, false);
  assert.equal(nodeByTag(result.nodes, 'div').reason, 'reserved_attribute');
});

test('selection after insert resolves the inserted node start in UTF-8 bytes', () => {
  const files = [{ path: 'A.jsx', source: `<div><p>🌍</p></div>` }];
  const div = nodeByTag(
    mapDocuments({ framework: 'react', files, revision: REVISION }).nodes,
    'div',
  );
  const { result, output } = appendInserted(
    files,
    { kind: 'insert', target: div.id, placement: 'inside', component: 'text' },
    `<p>new</p>`,
    'react',
    REVISION,
  );
  const insertedSource = output.get('A.jsx');
  const buffer = Buffer.from(insertedSource, 'utf8');
  assert.equal(buffer.subarray(result.selection.start).toString('utf8').startsWith('<p>new</p>'), true);
  assert.equal(result.selection.path, 'A.jsx');
});
