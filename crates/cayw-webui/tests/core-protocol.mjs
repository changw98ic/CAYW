import assert from 'node:assert/strict';
import test from 'node:test';
import { applySnapshot, beginWrite, consumeEvents, createView, finishWrite, messagePresentation } from '../assets/app.js';

const lease = { session_id: 'core3-session', attempt_id: 'attempt-1', worker_id: 'worker-1', fence: 1 };
const snapshot = { id: lease.session_id, state: 'idle', cursor: 5, attempt: null, messages: [], pending_approvals: [], unresolved_calls: [] };

function finishedCancel(protocol, eventLease = lease, envelopeLease = lease, receiptError = null) {
  let view = createView(lease.session_id);
  view = applySnapshot(view, { ...snapshot, cursor: 1, state: 'running', attempt: lease }, true);
  view = beginWrite(view, 'cancel-1', 'session.cancel', { worker_id: lease.worker_id, attempt_id: lease.attempt_id, fence: lease.fence });
  view = finishWrite(view, 'cancel-1', receiptError, receiptError ? null : { cancelled: true });
  view = consumeEvents(view, [{ protocol_version: protocol, session_id: lease.session_id, cursor: 5, attempt: envelopeLease, event: { type: 'attempt_finished', lease: eventLease } }]);
  return applySnapshot(view, snapshot, false, 5, view.proofEpoch);
}

test('the original Core v3 terminal settles a successful cancel receipt', () => {
  assert.equal(finishedCancel(3).writes.length, 0);
});

test('an obsolete Core terminal cannot settle the current cancel receipt', () => {
  assert.equal(finishedCancel(1).writes.length, 1);
});

test('a cancel target that differs from the observed attempt keeps its own receipt obligation', () => {
  const other = { ...lease, worker_id: 'worker-2', fence: 2 };
  let view = createView(lease.session_id);
  view = applySnapshot(view, { ...snapshot, cursor: 1, state: 'running', attempt: lease }, true);
  view = beginWrite(view, 'cancel-1', 'session.cancel', { worker_id: other.worker_id, attempt_id: other.attempt_id, fence: other.fence });
  view = finishWrite(view, 'cancel-1', null, { cancelled: true });
  view = consumeEvents(view, [{ protocol_version: 3, session_id: lease.session_id, cursor: 5, attempt: lease, event: { type: 'attempt_finished', lease } }]);
  view = applySnapshot(view, snapshot, false, 5, view.proofEpoch);
  assert.equal(view.writes.length, 1);
});

test('a terminal must retain the original worker and fence', () => {
  const otherWorker = { ...lease, worker_id: 'worker-2', fence: 2 };
  assert.equal(finishedCancel(3, otherWorker, otherWorker).writes.length, 1);
});

test('a terminal lease must match its original event envelope', () => {
  assert.equal(finishedCancel(3, lease, { ...lease, fence: 2 }).writes.length, 1);
});

test('a terminal does not settle an uncertain cancel request', () => {
  const error = { code: 'transport_error', message: 'receipt lost', outcome_unknown: true };
  const view = finishedCancel(3, lease, lease, error);
  assert.equal(view.writes.length, 1);
  assert.equal(view.writes[0].phase, 'uncertain');
});

test('completed file output shows returned content without changing the raw record', () => {
  const message = { role: 'toolResult', tool_name: 'execute_code', content: [{ type: 'text', text: '{"status":"completed"}' }], structured_content: { status: 'completed', outputs: [{ output: { type: 'json', value: { path: 'result.txt', content: '实际返回内容' } } }] } };
  const presentation = messagePresentation(message);
  assert.equal(presentation.title, '执行完成');
  assert.deepEqual(presentation.values, [{ label: 'result.txt', text: '实际返回内容' }]);
  assert.equal(presentation.raw, '{"status":"completed"}');
});

test('unknown effect takes precedence over a completed page and hides success output', () => {
  const presentation = messagePresentation({ role: 'toolResult', tool_name: 'execute_code', outcome_unknown: true, structured_content: { status: 'completed', outputs: [{ output: { type: 'json', value: { content: 'not a confirmed effect' } } }] } });
  assert.equal(presentation.status, 'unknown');
  assert.equal(presentation.title, '执行结果待核对');
  assert.deepEqual(presentation.values, []);
});

test('a stopped page is displayed as stopped even when its tool receipt has is_error', () => {
  const presentation = messagePresentation({ role: 'toolResult', tool_name: 'execute_code', is_error: true, structured_content: { status: 'cancelled', outputs: [], error: 'Cancelled: CodeMode execution cancelled' } });
  assert.equal(presentation.title, '已停止');
});

test('unrecognized tool output is a record with no invented success', () => {
  const presentation = messagePresentation({ role: 'toolResult', tool_name: 'execute_code', content: [{ type: 'text', text: '{"future_result":true}' }] });
  assert.equal(presentation.status, 'record');
  assert.equal(presentation.title, '工具返回了执行记录');
  assert.equal(presentation.raw, '{"future_result":true}');
});

test('another tool cannot impersonate a known execution page', () => {
  const presentation = messagePresentation({ role: 'toolResult', tool_name: 'different_tool', structured_content: { status: 'completed', outputs: [] } });
  assert.equal(presentation.status, 'record');
  assert.equal(presentation.title, '工具返回了执行记录');
});

test('ordinary assistant JSON stays ordinary assistant text', () => {
  const text = 'Offline fixture: {"status":"completed"}';
  const presentation = messagePresentation({ role: 'assistant', provider: 'real-provider', model: 'actual-model', content: [{ type: 'text', text }] });
  assert.equal(presentation.kind, 'text');
  assert.equal(presentation.text, text);
});
