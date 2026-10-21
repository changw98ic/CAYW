// Run with: node --test crates/cayw-webui/tests/ui.test.mjs
// Uses Node's built-in runner; no npm install or browser globals are required.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const source = await readFile(new URL('../assets/app.js', import.meta.url), 'utf8');
const { createView, consumeEvents, applySnapshot, sessionBlocked, messageText, summarizeProjects, summarizePlugins } = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

const snapshot = (extra = {}) => ({ id: 's1', label: '会话', messages: [], cursor: 7, state: 'idle', pending_approvals: [], unresolved_calls: [], ...extra });

// Resetting cursor on reconnection would duplicate event output or skip unread records.
test('resumes at the last consumed event and ignores replayed records', () => {
  let view = applySnapshot(createView('s1'), snapshot(), true);
  view = consumeEvents(view, [{ cursor: 8, event: { type: 'text_delta', delta: '你' } }]);
  view = consumeEvents(view, [{ cursor: 8, event: { type: 'text_delta', delta: '你' } }, { cursor: 9, event: { type: 'text_delta', delta: '好' } }]);
  assert.equal(view.cursor, 9);
  assert.equal(view.liveText, '你好');
  assert.equal(view.events.length, 2);
});

test('malformed or out-of-order events do not advance the existing cursor', () => {
  const view = applySnapshot(createView('s1'), snapshot(), true);
  assert.throws(() => consumeEvents(view, [{ cursor: 8, event: {} }, { cursor: 'bad', event: {} }]));
  assert.throws(() => consumeEvents(view, [{ cursor: 9, event: {} }, { cursor: 8, event: {} }]));
  assert.equal(view.cursor, 7);
  assert.equal(view.events.length, 0);
});

// A runtime event that does not declare text must not be presented as model streaming.
test('unknown events stay visible as records and never become assistant text', () => {
  const view = consumeEvents(createView('s1'), [{ cursor: 1, event: { type: 'new_runtime_event', text: 'not a text delta' } }]);
  assert.equal(view.liveText, '');
  assert.equal(view.events[0].cursor, 1);
});

test('agent partial text replaces the preview instead of duplicating deltas', () => {
  let view = consumeEvents(createView('s1'), [{ cursor: 1, event: { type: 'message_update', message: { content: [{ type: 'text', text: '你好' }] }, event: { type: 'text_delta', delta: '好' } } }]);
  assert.equal(view.liveText, '你好');
  view = consumeEvents(view, [{ cursor: 2, event: { type: 'message_end', message: { role: 'assistant', content: [{ type: 'text', text: '你好' }] } } }]);
  assert.equal(view.liveText, '');
});

test('refreshing a snapshot does not skip events that have not been consumed', () => {
  const view = applySnapshot(applySnapshot(createView('s1'), snapshot(), true), snapshot({ cursor: 12 }));
  assert.equal(view.cursor, 7);
  assert.throws(() => applySnapshot(view, snapshot({ id: 'other' })));
});

test('history and event previews are bounded and preserve recent entries', () => {
  const records = Array.from({ length: 400 }, (_, i) => ({ cursor: i + 1, event: { type: 'text_delta', delta: 'x'.repeat(1000) } }));
  const view = consumeEvents(createView('s1'), records);
  assert.ok(view.events.length <= 120);
  assert.equal(view.events.at(-1).cursor, 400);
  assert.ok(view.liveText.length <= 64000);
  const populated = applySnapshot(view, snapshot({ messages: Array.from({ length: 300 }, (_, i) => ({ role: 'user', content: [{ type: 'text', text: String(i) }] })) }));
  assert.ok(populated.snapshot.messages.length <= 120);
  assert.equal(messageText(populated.snapshot.messages.at(-1)), '299');
});

test('unknown side effects and unavailable workflow block prompts without clearing evidence', () => {
  assert.match(sessionBlocked(snapshot({ unresolved_calls: [{ id: 'tool-1', outcome_unknown: true }] })), /核对/);
  assert.match(sessionBlocked(snapshot({ state: 'frozen' })), /冻结/);
  assert.equal(sessionBlocked(snapshot()), '');
});

test('summarizeProjects aggregates per-project session and safety states', () => {
  const projects = [
    { id: 'default', label: '主项目', address: '127.0.0.1:4100', default: true },
    { id: 'docs', label: '文档项目', address: '127.0.0.1:4101', default: false },
  ];
  const sessions = {
    default: [snapshot({ id: 's1', state: 'running', attempt: { attempt_id: 'a1' } })],
    docs: [snapshot({ id: 's2', state: 'needs_reconciliation', unresolved_calls: [{ id: 'c1', state: 'outcome_unknown' }] })],
  };
  const result = summarizeProjects(projects, sessions, {});
  assert.equal(result.length, 2);
  assert.equal(result[0].sessionCount, 1);
  assert.match(result[0].statusLabel, /运行中/);
  assert.equal(result[1].unresolvedCount, 1);
  assert.match(result[1].statusLabel, /待核对副作用/);
});

test('summarizePlugins reports bound plugins and blocks fork when reconciliation is required', () => {
  const frozenSession = snapshot({
    id: 's-frozen',
    label: '强流程会话',
    binding: {
      plugin: 'strongflow',
      version: '1.2.0',
      registry_id: 'reg-1',
      generation: 3,
      frozen: true,
      reason: 'plugin strongflow was unloaded by the host',
      manifest: { capabilities: [{ name: 'strongflow.step' }] },
    },
  });
  const unknownSession = snapshot({
    id: 's-unknown',
    label: '待核对会话',
    state: 'needs_reconciliation',
    unresolved_calls: [{ call: { id: 'c1' }, state: 'outcome_unknown' }],
    binding: {
      plugin: 'strongflow',
      version: '1.2.0',
      registry_id: 'reg-1',
      generation: 3,
      frozen: true,
      reason: 'plugin unloaded',
      manifest: { capabilities: [] },
    },
  });
  const plugins = summarizePlugins([frozenSession, unknownSession], [{ name: 'jev.evaluate' }]);
  assert.equal(plugins.length, 2);
  const sf = plugins.find(item => item.name === 'strongflow');
  assert.equal(sf.frozen, true);
  assert.equal(sf.generation, 3);
  assert.deepEqual(sf.capabilities, ['strongflow.step']);
  assert.equal(sf.boundSessions.find(s => s.id === 's-frozen').canFork, true);
  assert.equal(sf.boundSessions.find(s => s.id === 's-unknown').canFork, false);
});
