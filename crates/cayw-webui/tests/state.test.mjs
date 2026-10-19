import test from 'node:test';
import assert from 'node:assert/strict';
import { createView, applySnapshot, consumeEvents, sessionBlocked, sessionRunning, messageText, beginWrite, finishWrite, terminalReadCursor, consumeTerminalRead } from '../assets/app.js';

const snapshot = (extra = {}) => ({ id: 's1', label: '测试', messages: [], cursor: 10, state: 'idle', pending_approvals: [], unresolved_calls: [], ...extra });
const initial = (extra = {}) => applySnapshot(createView('s1'), snapshot(extra), true);

test('snapshot refresh does not skip events that have not been consumed', () => {
  let view = initial();
  view = applySnapshot(view, snapshot({ cursor: 18, messages: [{ role: 'user', content: [{ type: 'text', text: '你好' }] }] }));
  assert.equal(view.cursor, 10);
  assert.equal(messageText(view.snapshot.messages[0]), '你好');
  view = consumeEvents(view, [{ cursor: 11, event: { type: 'message_update', message: { content: [{ type: 'text', text: '回答' }] } } }]);
  assert.equal(view.cursor, 11);
  assert.equal(view.liveText, '回答');
});

test('a delayed snapshot cannot replace newer session history or blocking state', () => {
  let view = initial();
  view = applySnapshot(view, snapshot({ cursor: 18, state: 'needs_reconciliation', messages: [{ role: 'assistant', content: '真实终态' }] }));
  view = applySnapshot(view, snapshot({ cursor: 12, state: 'running', messages: [] }));
  assert.equal(view.snapshot.cursor, 18);
  assert.equal(view.snapshot.state, 'needs_reconciliation');
  assert.equal(messageText(view.snapshot.messages[0]), '真实终态');
});

test('duplicate events are ignored and malformed pages leave the original view untouched', () => {
  let view = initial();
  view = consumeEvents(view, [{ cursor: 11, event: { type: 'future_extension', text: '<img onerror=alert(1)>' } }]);
  view = consumeEvents(view, [{ cursor: 11, event: { type: 'future_extension' } }]);
  assert.equal(view.events.length, 1);
  assert.equal(view.liveText, '');
  assert.throws(() => consumeEvents(view, [{ cursor: 12, event: {} }, { cursor: 'bad', event: {} }]));
  assert.equal(view.cursor, 11);
  assert.equal(view.events.length, 1);
});

test('unsafe and decreasing cursors reject the entire page', () => {
  const view = initial();
  assert.throws(() => consumeEvents(view, [{ cursor: 14, event: {} }, { cursor: 13, event: {} }]));
  assert.throws(() => consumeEvents(view, [{ cursor: 9007199254740992, event: {} }]));
  assert.equal(view.cursor, 10);
});

test('render history and streamed content stay bounded while the cursor consumes the whole page', () => {
  let view = initial({ messages: Array.from({ length: 900 }, (_, i) => ({ role: 'user', content: [{ type: 'text', text: String(i) }] })) });
  view = consumeEvents(view, Array.from({ length: 900 }, (_, i) => ({ cursor: i + 11, event: { type: 'text_delta', delta: 'x'.repeat(200) } })));
  assert.equal(view.snapshot.messages.length, 120);
  assert.equal(messageText(view.snapshot.messages.at(-1)), '899');
  assert.equal(view.events.length, 120);
  assert.ok(view.liveText.length <= 64000);
  assert.equal(view.cursor, 910);
});

test('unknown effects remain blocked and frozen sessions explain the confirmed fork route', () => {
  assert.match(sessionBlocked(snapshot({ unresolved_calls: [{ id: 'c1' }] })), /核对/);
  assert.match(sessionBlocked(snapshot({ needs_reconciliation: true })), /核对/);
  assert.match(sessionBlocked(snapshot({ state: 'needs_reconciliation' })), /核对/);
  assert.match(sessionBlocked(snapshot({ state: 'frozen', workflow_unavailable: true })), /确认后 Fork/);
});

test('unfinished known calls keep approval available while their durable attempt is active', () => {
  const awaiting = snapshot({ state:'awaiting_approval', attempt:{attempt_id:'a1'}, unresolved_calls:[{state:'started',execution_started:true,result:null}], pending_approvals:[{id:'approval-1'}] });
  assert.equal(sessionBlocked(awaiting), '');
  assert.equal(sessionRunning(awaiting), true);
  assert.match(sessionBlocked({...awaiting, unresolved_calls:[{state:'outcome_unknown'}]}), /核对/);
});

test('an active durable attempt blocks sending even before its first running projection', () => {
  assert.equal(sessionRunning(snapshot({ attempt: { session_id:'s1', attempt_id:'a1', worker_id:'w1', fence:1 } })), true);
  assert.equal(sessionRunning(snapshot()), false);
});

test('an unrecognized runtime state never enables a new prompt', () => {
  assert.match(sessionBlocked(snapshot({ state:'future_state' })), /状态/);
  assert.match(sessionBlocked(snapshot({ state:null })), /状态/);
});

test('frozen binding is visible even when runtime returns the durable state as idle', () => {
  assert.match(sessionBlocked(snapshot({ state:'idle', binding:{ plugin:'strongflow', frozen:true } })), /冻结/);
});

test('wrapped durable agent updates drive live text and their terminal clears it', () => {
  let view = initial();
  view = consumeEvents(view, [{cursor:11, event:{type:'agent', event:{type:'message_update', message:{content:[{type:'text',text:'逐步回答'}]}}}}]);
  assert.equal(view.liveText, '逐步回答');
  view = consumeEvents(view, [{cursor:12, event:{type:'agent', event:{type:'agent_end',messages:[]}}}]);
  assert.equal(view.liveText, '');
});

test('tool errors and unknown outcomes are visible even when the content is empty', () => {
  assert.match(messageText({ role: 'toolResult', content: [], is_error: true, outcome_unknown: true }), /待核对/);
  assert.match(messageText({ role: 'toolResult', content: [], is_error: true }), /失败/);
});

test('model content remains literal text', () => {
  assert.equal(messageText({ role: 'assistant', content: [{ type: 'text', text: '<script>alert(1)</script>' }] }), '<script>alert(1)</script>');
});

const lease = { session_id:'s1', attempt_id:'a1', worker_id:'w1', fence:1 };
const fact = (cursor, event) => ({ protocol_version:3, session_id:'s1', cursor, timestamp:1, attempt:lease, event });

test('same-cursor idle cannot erase a durable reconciliation warning before its fact is consumed', () => {
  let view = initial({ state:'needs_reconciliation', cursor:12, unresolved_calls:[{ call:{id:'c1'}, state:'outcome_unknown' }] });
  view = applySnapshot(view, snapshot({cursor:12}));
  assert.match(sessionBlocked(view.snapshot, view), /核对/);
  view = applySnapshot(view, snapshot({cursor:13}));
  assert.match(sessionBlocked(view.snapshot, view), /核对/);
  view = consumeEvents(view, [fact(13, {type:'reconciled', reconciliation:{call_id:'c1'}})]);
  view = applySnapshot(view, snapshot({cursor:13}));
  assert.equal(sessionBlocked(view.snapshot, view), '');
});

test('a known prompt receipt cannot unlock against a read issued before consuming its completion', () => {
  let view = beginWrite(initial(), 1, 'session.prompt');
  view = finishWrite(view, 1, null, snapshot({cursor:13}));
  view = applySnapshot(view, snapshot({cursor:13}));
  view = consumeEvents(view, [fact(11,{type:'attempt_started',lease}), fact(13,{type:'attempt_finished',lease})]);
  assert.match(sessionBlocked(view.snapshot, view), /结束结果/);
  view = applySnapshot(view, snapshot({cursor:13}));
  assert.equal(sessionBlocked(view.snapshot, view), '');
});

test('approval terminal releases only that decision so the next approval can be presented', () => {
  let view = initial({state:'awaiting_approval',attempt:lease,pending_approvals:[{id:'p1'}]});
  view = beginWrite(view, 1, 'approval.decide', {approval_id:'p1'});
  view = finishWrite(view, 1);
  view = consumeEvents(view, [fact(11,{type:'approval_decided',approval:{id:'p1',attempt_id:'a1'}})]);
  view = applySnapshot(view,snapshot({cursor:11,state:'awaiting_approval',attempt:lease,pending_approvals:[{id:'p2'}]}));
  assert.equal(view.writes.length,0);
  assert.equal(sessionRunning(view.snapshot),true);
  assert.equal(view.snapshot.pending_approvals[0].id,'p2');
});

test('a late write response cannot use a snapshot fetched before consuming its terminal', () => {
  let view = beginWrite(initial(),1,'session.prompt');
  view = applySnapshot(view,snapshot({cursor:12}));
  view = consumeEvents(view,[fact(11,{type:'attempt_started',lease}),fact(12,{type:'attempt_finished',lease})]);
  view = finishWrite(view,1,null,snapshot({cursor:12}));
  assert.match(sessionBlocked(view.snapshot,view),/结束结果/);
  view = applySnapshot(view,snapshot({cursor:12}));
  assert.equal(sessionBlocked(view.snapshot,view),'');
});

test('an uncertain fork cannot settle on another execution in its source session', () => {
  let view = beginWrite(initial({binding:{plugin:'strongflow',frozen:true}}),1,'session.fork');
  view = finishWrite(view,1,{code:'transport_unknown',outcome_unknown:true});
  view = consumeEvents(view,[fact(11,{type:'attempt_started',lease}),fact(12,{type:'attempt_finished',lease})]);
  view = applySnapshot(view,snapshot({cursor:12,binding:{plugin:'strongflow',frozen:true}}));
  assert.equal(view.writes.length,1);
  assert.match(sessionBlocked(view.snapshot,view),/确认/);
});

test('an unknown prompt cannot inherit an existing or subsequently observed execution', () => {
  let view = beginWrite(initial({state:'running', attempt:lease}), 1, 'session.prompt');
  view = finishWrite(view, 1, {code:'transport_unknown', outcome_unknown:true});
  view = consumeEvents(view, [fact(11, {type:'attempt_finished', lease})]);
  view = applySnapshot(view, snapshot({cursor:11}));
  assert.equal(view.writes.length, 1);
  view = consumeEvents(view, [fact(12, {type:'attempt_started', lease}), fact(13, {type:'attempt_finished', lease})]);
  view = applySnapshot(view, snapshot({cursor:13}));
  assert.equal(view.writes.length, 1);
});

for (const order of ['receipt-first', 'terminal-first']) {
  test(`a no-op cancel settles only itself with ${order}`, () => {
    let view = beginWrite(initial(), 1, 'session.prompt');
    const completed = snapshot({cursor:12});
    if (order === 'terminal-first') {
      view = consumeEvents(view, [fact(11, {type:'attempt_started', lease}), fact(12, {type:'attempt_finished', lease})]);
      view = applySnapshot(view, completed);
    }
    view = beginWrite(view, 2, 'session.cancel');
    view = finishWrite(view, 2, null, {cancelled:false});
    assert.equal(view.writes.length, 1, 'no-op cancel has no outstanding effect, original prompt still does');
    view = finishWrite(view, 1, null, completed);
    if (order === 'receipt-first') {
      view = consumeEvents(view, [fact(11, {type:'attempt_started', lease}), fact(12, {type:'attempt_finished', lease})]);
    }
    view = applySnapshot(view, completed);
    assert.equal(sessionBlocked(view.snapshot, view), '');
  });
}

test('a known unrelated no-op cancel cannot erase an uncorrelated prompt', () => {
  let view = finishWrite(beginWrite(initial(), 1, 'session.prompt'), 1, {code:'transport_unknown', outcome_unknown:true});
  view = finishWrite(beginWrite(view, 2, 'session.cancel'), 2, null, {cancelled:false});
  view = applySnapshot(view, snapshot());
  assert.equal(view.writes.length, 1);
  assert.equal(view.writes[0].key, 1);
  assert.match(sessionBlocked(view.snapshot, view), /确认/);
});

test('a lost cancel receipt remains blocked after the original prompt succeeds', () => {
  let view = beginWrite(initial(), 1, 'session.prompt');
  view = consumeEvents(view, [fact(11, {type:'attempt_started', lease})]);
  view = applySnapshot(view, snapshot({cursor:11, state:'running', attempt:lease}));
  view = beginWrite(view, 2, 'session.cancel');
  view = finishWrite(view, 2, {code:'transport_unknown', outcome_unknown:true});
  view = consumeEvents(view, [fact(12, {type:'attempt_finished', lease})]);
  view = finishWrite(view, 1, null, snapshot({cursor:12}));
  view = applySnapshot(view, snapshot({cursor:12}));
  assert.equal(view.writes.length, 1, 'a request whose dispatch is unknown needs its own receipt');
  assert.equal(view.writes[0].key, 2);
});

for (const order of ['receipt-first', 'events-first', 'cancel-receipt-last']) {
  test(`an early true cancel uses its own pending prompt completion with ${order}`, () => {
    let view = beginWrite(initial(), 1, 'session.prompt');
    view = beginWrite(view, 2, 'session.cancel');
    const complete = snapshot({cursor:12});
    if (order !== 'cancel-receipt-last') view = finishWrite(view, 2, null, {cancelled:true});
    if (order === 'receipt-first') view = finishWrite(view, 1, null, complete);
    view = consumeEvents(view, [fact(11, {type:'attempt_started', lease}), fact(12, {type:'attempt_finished', lease})]);
    view = applySnapshot(view, complete, false, 10);
    if (order !== 'receipt-first') view = finishWrite(view, 1, null, complete);
    assert.match(sessionBlocked(view.snapshot, view), /结束结果/, 'a read started before consuming the terminal is insufficient');
    view = applySnapshot(view, complete);
    if (order === 'cancel-receipt-last') {
      assert.match(sessionBlocked(view.snapshot, view), /结束结果/, 'cancel still needs its own known receipt');
      view = finishWrite(view, 2, null, {cancelled:true});
    }
    assert.equal(sessionBlocked(view.snapshot, view), '');
    assert.equal(view.writes.length, 0);
  });
}

test('an early true cancel cannot use an unrelated or missing prompt terminal', () => {
  let view = beginWrite(initial(), 1, 'session.prompt');
  view = finishWrite(beginWrite(view, 2, 'session.cancel'), 2, null, {cancelled:true});
  const unrelated = {...lease, attempt_id:'other-client'};
  view = consumeEvents(view, [fact(11, {type:'attempt_started', lease:unrelated}), fact(12, {type:'attempt_finished', lease:unrelated})]);
  view = applySnapshot(view, snapshot({cursor:12}));
  assert.match(sessionBlocked(view.snapshot, view), /结束结果/);
  view = finishWrite(view, 1, null, snapshot({cursor:14}));
  view = consumeEvents(view, [fact(14, {type:'agent', event:{type:'agent_end', messages:[]}})]);
  view = applySnapshot(view, snapshot({cursor:14}));
  assert.match(sessionBlocked(view.snapshot, view), /结束结果/, 'a successful prompt cursor without its exact attempt terminal is insufficient');
});

test('an early cancel keeps both unknown receipt obligations separate', () => {
  for (const unknown of ['session.prompt', 'session.cancel']) {
    let view = beginWrite(initial(), 1, 'session.prompt');
    view = beginWrite(view, 2, 'session.cancel');
    view = finishWrite(view, 2, unknown === 'session.cancel' ? {code:'transport_unknown', outcome_unknown:true} : null, {cancelled:true});
    view = consumeEvents(view, [fact(11, {type:'attempt_started', lease}), fact(12, {type:'attempt_finished', lease})]);
    view = finishWrite(view, 1, unknown === 'session.prompt' ? {code:'transport_unknown', outcome_unknown:true} : null, snapshot({cursor:12}));
    view = applySnapshot(view, snapshot({cursor:12}));
    assert.match(sessionBlocked(view.snapshot, view), /确认/, `${unknown} still needs its own receipt`);
  }
});

test('an early true cancel does not borrow a different local prompt receipt', () => {
  let view = beginWrite(initial(), 1, 'session.prompt');
  view = finishWrite(beginWrite(view, 2, 'session.cancel'), 2, null, {cancelled:true});
  view = finishWrite(view, 1, {code:'busy', outcome_unknown:false});
  view = beginWrite(view, 3, 'session.prompt');
  view = consumeEvents(view, [fact(11, {type:'attempt_started', lease}), fact(12, {type:'attempt_finished', lease})]);
  view = finishWrite(view, 3, null, snapshot({cursor:12}));
  view = applySnapshot(view, snapshot({cursor:12}));
  assert.match(sessionBlocked(view.snapshot, view), /结束结果/);
  assert.equal(view.writes.length, 1);
  assert.equal(view.writes[0].key, 2);
});

test('a malformed early cancel receipt cannot borrow a successful prompt completion', () => {
  let view = beginWrite(initial(), 1, 'session.prompt');
  view = finishWrite(beginWrite(view, 2, 'session.cancel'), 2, null, {});
  view = consumeEvents(view, [fact(11, {type:'attempt_started', lease}), fact(12, {type:'attempt_finished', lease})]);
  view = finishWrite(view, 1, null, snapshot({cursor:12}));
  view = applySnapshot(view, snapshot({cursor:12}));
  assert.match(sessionBlocked(view.snapshot, view), /结束结果/);
});

test('delayed snapshots cannot recreate a completed attempt or undo reconciled calls', () => {
  const call = id => ({call:{id}, state:'outcome_unknown', result:{outcome_unknown:true}});
  let view = initial({cursor:12, state:'needs_reconciliation', unresolved_calls:[call('c1'), call('c2')]});
  view = consumeEvents(view, [fact(13, {type:'reconciled', reconciliation:{call_id:'c1'}})]);
  view = applySnapshot(view, snapshot({cursor:12, state:'needs_reconciliation', unresolved_calls:[call('c1'), call('c2')]}));
  view = applySnapshot(view, snapshot({cursor:13, state:'needs_reconciliation', unresolved_calls:[call('c2')]}));
  view = consumeEvents(view, [fact(14, {type:'reconciled', reconciliation:{call_id:'unrelated'}})]);
  view = applySnapshot(view, snapshot({cursor:14}));
  assert.match(sessionBlocked(view.snapshot, view), /核对/, 'c2 still lacks its own reconciliation');
  view = consumeEvents(view, [fact(15, {type:'reconciled', reconciliation:{call_id:'c2'}})]);
  view = applySnapshot(view, snapshot({cursor:14, state:'needs_reconciliation', unresolved_calls:[call('c2')]}));
  view = applySnapshot(view, snapshot({cursor:15}));
  assert.equal(sessionBlocked(view.snapshot, view), '');
  view = consumeEvents(view, [fact(16, {type:'attempt_started', lease}), fact(17, {type:'attempt_finished', lease})]);
  view = applySnapshot(view, snapshot({cursor:16, state:'running', attempt:lease}));
  view = applySnapshot(view, snapshot({cursor:17}));
  assert.equal(sessionBlocked(view.snapshot, view), '', 'the old running read must not resurrect a consumed terminal');
});

test('a stale same-cursor read cannot revoke snapshot coverage before a late receipt', () => {
  let view = beginWrite(initial(), 1, 'session.prompt');
  view = consumeEvents(view, [fact(11, {type:'attempt_started', lease}), fact(12, {type:'attempt_finished', lease})]);
  view = applySnapshot(view, snapshot({cursor:12}), false, 12);
  view = applySnapshot(view, snapshot({cursor:12}), false, 10);
  view = finishWrite(view, 1, null, snapshot({cursor:12}));
  assert.equal(sessionBlocked(view.snapshot, view), '');
});

test('historical proof waits for a snapshot requested after recovery, even at an unchanged forward cursor', () => {
  let view = beginWrite(initial(), 1, 'session.prompt');
  view = finishWrite(beginWrite(view, 2, 'session.cancel'), 2, null, {cancelled:true});
  const other = {...lease, attempt_id:'other-client', fence:2};
  view = consumeEvents(view, [fact(11, {type:'attempt_started', lease}), fact(12, {type:'attempt_finished', lease}), {...fact(13, {type:'attempt_started', lease:other}), attempt:other}, {...fact(14, {type:'attempt_finished', lease:other}), attempt:other}]);
  view = applySnapshot(view, snapshot({cursor:14}));
  view = finishWrite(view, 1, null, snapshot({cursor:12}));
  assert.equal(terminalReadCursor(view), 12);
  const requestedEpoch = view.proofEpoch;
  view = consumeTerminalRead(view, 12, [fact(12, {type:'attempt_finished', lease})]);
  assert.equal(terminalReadCursor(view), null);
  assert.equal(view.cursor, 14, 'historical evidence must not rewind or advance the forward cursor');
  view = applySnapshot(view, snapshot({cursor:14}), false, 14, requestedEpoch);
  assert.match(sessionBlocked(view.snapshot, view), /结束结果/, 'an in-flight read cannot cover newly recovered evidence');
  view = applySnapshot(view, snapshot({cursor:14}));
  assert.equal(sessionBlocked(view.snapshot, view), '');
  assert.equal(view.writes.length, 0);
  view = beginWrite(view, 3, 'session.prompt');
  view = consumeTerminalRead(view, 12, [fact(12, {type:'attempt_finished', lease})]);
  view = finishWrite(view, 1, null, snapshot({cursor:12}));
  view = finishWrite(view, 2, null, {cancelled:true});
  assert.equal(view.writes.length, 1, 'late duplicate receipts/proofs cannot release a later write');
  assert.equal(view.writes[0].key, 3);
});

test('unknown prompt and cancel receipts do not schedule historical guesses', () => {
  for (const unknown of [1, 2]) {
    let view = beginWrite(initial(), 1, 'session.prompt');
    view = beginWrite(view, 2, 'session.cancel');
    const other = {...lease, attempt_id:'other-client', fence:2};
    view = consumeEvents(view, [fact(11, {type:'attempt_started', lease}), fact(12, {type:'attempt_finished', lease}), {...fact(13, {type:'attempt_started', lease:other}), attempt:other}, {...fact(14, {type:'attempt_finished', lease:other}), attempt:other}]);
    view = finishWrite(view, 1, unknown === 1 ? {code:'transport_unknown', outcome_unknown:true} : null, snapshot({cursor:12}));
    view = finishWrite(view, 2, unknown === 2 ? {code:'transport_unknown', outcome_unknown:true} : null, {cancelled:true});
    assert.equal(terminalReadCursor(view), null);
    view = applySnapshot(view, snapshot({cursor:14}));
    assert.match(sessionBlocked(view.snapshot, view), /确认/);
  }
});

test('a reconciliation rejection with no known call cannot be cleared by an unrelated call', () => {
  let view = finishWrite(beginWrite(initial(), 1, 'session.prompt'), 1, {code:'needs_reconciliation', outcome_unknown:false});
  view = consumeEvents(view, [fact(11, {type:'reconciled', reconciliation:{call_id:'not-this-call'}})]);
  view = applySnapshot(view, snapshot({cursor:11}));
  assert.match(sessionBlocked(view.snapshot, view), /核对/);
});
