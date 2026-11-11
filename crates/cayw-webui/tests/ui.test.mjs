// Run with: node --test crates/cayw-webui/tests/ui.test.mjs
// Uses Node's built-in runner; no npm install or browser globals are required.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const source = await readFile(new URL('../assets/app.js', import.meta.url), 'utf8');
const { createView, consumeEvents, applySnapshot, sessionBlocked, messageText, beginWrite, finishWrite, selectActiveTaskId, summarizeRestoreReport } = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

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

test('interrupted tool effects never block prompts while an unavailable workflow stays frozen', () => {
  assert.equal(sessionBlocked(snapshot({ unresolved_calls: [{ id: 'tool-1', outcome_unknown: true }] })), '');
  assert.match(sessionBlocked(snapshot({ state: 'frozen' })), /冻结/);
  assert.equal(sessionBlocked(snapshot()), '');
});

test('second external task replaces first external task id while owned prompts retain their task id', () => {
  const lease1 = { session_id: 's1', attempt_id: 'a1', worker_id: 'w1', fence: 1 };
  const lease2 = { session_id: 's1', attempt_id: 'a2', worker_id: 'w1', fence: 2 };
  let view = applySnapshot(createView('s1'), snapshot({ cursor: 10, state: 'running', attempt: lease1 }), true);
  view = { ...view, originalTaskId: 'ext-1' };

  // Directly transitioning from external task 1 to external task 2 (without observing idle) drops stale ext-1.
  view = applySnapshot(view, snapshot({
    cursor: 12,
    state: 'running',
    attempt: lease2,
    unresolved_calls: [{ call: { id: 'old-call', task_id: 'ext-1' }, state: 'outcome_unknown', result: { outcome_unknown: true } }],
  }));
  assert.equal(view.originalTaskId, undefined);

  const tasks = [
    { id: 'ext-1', session_id: 's1', state: 'cancelled', attempt_id: 'a1' },
    { id: 'ext-2', session_id: 's1', state: 'running', attempt_id: 'a2' },
  ];
  assert.equal(selectActiveTaskId(tasks, 's1', view.snapshot), 'ext-2');

  // Even when external task 1 is still draining and external task 2 is waiting_approval or queued, stop targets task 2.
  assert.equal(
    selectActiveTaskId(
      [
        { id: 'ext-1', session_id: 's1', state: 'draining', attempt_id: 'a1' },
        { id: 'ext-2', session_id: 's1', state: 'queued', attempt_id: null },
      ],
      's1',
      snapshot({ cursor: 13, state: 'running', attempt: lease1 })
    ),
    'ext-2'
  );
  assert.equal(
    selectActiveTaskId(
      [
        { id: 'ext-1', session_id: 's1', state: 'completed', attempt_id: 'a1' },
        { id: 'ext-2', session_id: 's1', state: 'waiting_approval', attempt_id: null },
      ],
      's1',
      snapshot({ cursor: 14, state: 'awaiting_approval', pending_approvals: [{ id: 'ap-2', input: { task_id: 'ext-2' } }] })
    ),
    'ext-2'
  );

  // Owned local prompt keeps its originalTaskId across snapshot updates until its write settles.
  let owned = beginWrite(applySnapshot(createView('s1'), snapshot({ cursor: 20 }), true), 1, 'session.prompt', { session_id: 's1', text: 'hi' });
  owned = { ...owned, originalTaskId: 'owned-1' };
  owned = applySnapshot(owned, snapshot({ cursor: 21, state: 'running', attempt: lease2 }));
  assert.equal(owned.originalTaskId, 'owned-1');
});

test('restore report summary exposes unrestored conflicts, missing snapshot paths, unchanged files, and undo snapshot', () => {
  const blocked = summarizeRestoreReport({
    snapshot: 'abcdef1234567890abcdef1234567890abcdef12',
    restored: [],
    unchanged: ['README.md'],
    needs_confirmation: ['src/lib.rs'],
    not_in_snapshot: ['missing.txt'],
    undo_snapshot: '1111222233334444555566667777888899990000',
    complete: false,
  });
  assert.equal(blocked.complete, false);
  assert.deepEqual(blocked.needsConfirmation, ['src/lib.rs']);
  assert.deepEqual(blocked.notInSnapshot, ['missing.txt']);
  assert.deepEqual(blocked.unchanged, ['README.md']);
  assert.equal(blocked.undoSnapshot, '1111222233334444555566667777888899990000');
  assert.match(blocked.headline, /冲突/);
  assert.ok(blocked.details.some(d => d.kind === 'needs_confirmation' && d.paths.includes('src/lib.rs')));
  assert.ok(blocked.details.some(d => d.kind === 'not_in_snapshot' && d.paths.includes('missing.txt')));

  const partial = summarizeRestoreReport({
    snapshot: 'abcdef1234567890abcdef1234567890abcdef12',
    restored: ['src/lib.rs'],
    unchanged: ['README.md'],
    needs_confirmation: [],
    not_in_snapshot: ['missing.txt'],
    undo_snapshot: null,
    complete: true,
  });
  assert.equal(partial.complete, true);
  assert.match(partial.headline, /不在该恢复点中/);
  assert.deepEqual(partial.restored, ['src/lib.rs']);
  assert.deepEqual(partial.notInSnapshot, ['missing.txt']);
});

test('finished local prompt settles and drops stale originalTaskId even when an external task starts without idle gap', () => {
  const localLease = { session_id: 's1', attempt_id: 'a-local', worker_id: 'w1', fence: 1 };
  const extLease = { session_id: 's1', attempt_id: 'a-ext', worker_id: 'w1', fence: 2 };
  let view = applySnapshot(createView('s1'), snapshot({ cursor: 10, state: 'idle' }), true);
  view = beginWrite(view, 1, 'session.prompt', { session_id: 's1', text: 'local prompt' });
  view = { ...view, originalTaskId: 'local-task-1' };

  // Local prompt finishes at cursor 12.
  const promptResult = snapshot({ cursor: 12, state: 'idle', attempt: null });
  view = finishWrite(view, 1, null, promptResult);
  assert.equal(view.writes.length, 1, 'settling until events & snapshot cover cursor 12');

  // Before poll observes idle, an external task immediately starts at cursor 13 (`state: running`).
  view = consumeEvents(view, [
    { cursor: 11, event: { type: 'attempt_started', lease: localLease } },
    { cursor: 12, event: { type: 'attempt_finished', lease: localLease } },
    { cursor: 13, event: { type: 'attempt_started', lease: extLease } },
  ]);
  view = applySnapshot(view, snapshot({ cursor: 13, state: 'running', attempt: extLease }), false, 13, view.proofEpoch);
  assert.equal(view.writes.length, 0, 'local session.prompt write settles once its receipt cursor is covered');
  assert.equal(view.originalTaskId, undefined, 'stale local-task-1 is cleared instead of lingering onto extLease');
});

