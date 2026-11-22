const CORE_PROTOCOL_VERSION = 4;
const MAX_MESSAGES = 120;
const MAX_EVENTS = 120;
const clip = (value, limit = Number.POSITIVE_INFINITY) => {
  const text = String(value ?? '');
  return text.length > limit ? `${text.slice(0, limit)}\n…内容过长，已截断显示。` : text;
};
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const safeCursor = value => Number.isSafeInteger(value) && value >= 0;
const array = value => Array.isArray(value) ? value : [];
const recordText = value => JSON.stringify(value, null, 2) ?? '';

function renderLanNotice(open) {
  const target = document.getElementById('network-notice');
  if (!target) return;
  target.replaceChildren();
  if (open) { const note = document.createElement('p'); note.className = 'notice'; note.textContent = '局域网可以直接打开这个页面。如果要加账号，在运行 CAYW 的这台电脑上执行 cayw account。'; target.append(note); }
}

export function workspaceIdFromHash(hash) {
  if (typeof hash !== 'string') throw new Error('工作区链接片段无效。');
  const params = new URLSearchParams(hash.startsWith('#') ? hash.slice(1) : hash);
  const values = params.getAll('workspace');
  if (!values.length) return null;
  if (values.length !== 1) throw new Error('工作区链接只能包含一个 workspace。');
  const id = values[0];
  if (id === 'local' || /^workspace-[0-9a-f]{64}$/.test(id)) return id;
  throw new Error('工作区链接包含无效的本地工作区 ID。');
}

export function messageText(message) {
  if (!object(message)) return clip(message);
  const content = messageContent(message);
  const warning = message.outcome_unknown
    ? (message.is_error || !content ? '工具执行已中断。' : '')
    : message.is_error ? '工具执行失败。' : '';
  return clip([content, warning].filter(Boolean).join('\n'));
}

function messageContent(message) {
  if (typeof message.content === 'string') return clip(message.content);
  if (!Array.isArray(message.content)) return recordText(message.data ?? message);
  return clip(message.content.map(part => {
    if (part.type === 'text') return part.text ?? '';
    if (part.type === 'thinking') return `[思考]\n${part.thinking ?? ''}`;
    if (part.type === 'image') return `[图片：${part.mime_type ?? '未标注格式'}]`;
    if (part.type === 'toolCall') return `[工具调用：${part.name ?? '未命名'}]\n${recordText(part.arguments)}`;
    return recordText(part);
  }).join('\n'));
}

function textOnly(message) {
  return clip(array(message?.content).filter(part => part.type === 'text').map(part => part.text ?? '').join('\n'));
}

export function messagePresentation(message, execution = null) {
  const raw = message?.role === 'toolResult' ? null : messageText(message);
  if (message?.role === 'toolResult') {
    const original = message.structured_content;
    // Keep the original tool receipt intact. Later durable execution facts
    // update the card even when no model wait call returned a terminal page.
    const terminal = execution && execution.execution_id === original?.cell_id ? execution.terminal : null;
    const page = terminal && ['running', 'yielded'].includes(original?.status)
      ? { ...original, status: execution.state, vm_alive: false, error: terminal.error ?? terminal.reason ?? null }
      : original;
    const knownPage = message.tool_name === 'execute_code' && object(page) && ['completed', 'failed', 'cancelled', 'outcome_unknown', 'interrupted', 'running', 'yielded'].includes(page.status) && Array.isArray(page.outputs);
    const unknown = Boolean(message.outcome_unknown || (knownPage && (page.outcome_unknown || page.status === 'outcome_unknown')));
    const status = unknown ? 'outcome_unknown' : message.is_error && page?.status !== 'cancelled' ? 'failed' : knownPage ? page.status : 'record';
    const title = ({ completed: '执行完成', failed: '执行未完成', cancelled: '已停止', interrupted: '已中断', outcome_unknown: '结果待核对', running: '仍在执行', yielded: '等待继续', record: '工具返回了执行记录' })[status];
    const values = [];
    if (knownPage && status === 'completed') {
      for (const item of page.outputs.slice(0, 20)) {
        const output = item?.output;
        if (output?.type === 'text' && typeof output.text === 'string') values.push({ label: '输出', text: clip(output.text, 4000) });
        if (output?.type === 'json' && object(output.value) && typeof output.value.content === 'string') {
          values.push({ label: typeof output.value.path === 'string' ? clip(output.value.path, 200) : '返回内容', text: clip(output.value.content, 4000) });
        }
      }
    }
    let detail = '';
    if (unknown) detail = [page?.error, '执行可能已写入。请查看原始记录，核对后再决定是否重试。'].filter(Boolean).join('\n');
    else if (status === 'interrupted') detail = page?.error || '本次工具执行已中断。';
    else if (status === 'cancelled') detail = '任务已停止。';
    else if (status === 'failed') {
      const reason = typeof page?.error === 'string' ? page.error : '';
      detail = reason || '执行未完成，可展开查看原因。';
    }
    return { kind: 'tool_result', title, status, text: detail, values, get raw() { return messageText(message); } };
  }
  const calls = array(message?.content).filter(part => part?.type === 'toolCall');
  if (message?.role === 'assistant' && calls.length) return { kind: 'tool_call', title: '执行工具', text: textOnly(message), calls, raw };
  if (message?.role === 'assistant' && message.provider === 'fixture' && message.model === 'offline-runtime' && raw.startsWith('Offline fixture: ')) {
    return { kind: 'record', title: '离线测试回复', text: '已收到离线测试的执行记录。', raw };
  }
  return { kind: 'text', text: raw, raw };
}

export function createView(id) {
  return { id, cursor: 0, snapshot: null, executions: {}, events: [], liveText: '', totalMessages: 0, snapshotAfter: 0, snapshotRequiredAfter: 0, proofEpoch: 0, snapshotEpoch: 0, writes: [], faults: {}, activeAttempt: null, approvalQuestions: {} };
}

const validLease = lease => object(lease) && ['session_id', 'attempt_id', 'worker_id'].every(key => typeof lease[key] === 'string' && lease[key].length > 0) && safeCursor(lease.fence) && lease.fence > 0;
const sameLease = (a, b) => validLease(a) && validLease(b) && ['session_id', 'attempt_id', 'worker_id', 'fence'].every(key => a[key] === b[key]);
const attemptTerminalLease = (id, record) => {
  if (record?.protocol_version !== CORE_PROTOCOL_VERSION || record.session_id !== id || !['attempt_finished', 'attempt_recovered'].includes(record.event?.type)) return null;
  const lease = record.event.lease;
  return lease?.session_id === id && sameLease(lease, record.attempt) ? lease : null;
};
const readMethods = new Set([
  'settings.workspace', 'settings.workspaces.list', 'settings.tools', 'settings.models.list', 'settings.plugins.list',
  'runtime.health',
  'session.list',
  'session.get',
  'session.events',
  'task.get',
  'capabilities.list',
  'session.tasks',
  'session.plan',
  'runtime.fleet',
  'session.context_overview',
  'session.maintenance.status',
  'session.run_inspector',
  'approval.question',
  'artifacts.list',
  'artifacts.preview',
  'artifacts.action_list',
  'artifacts.action_get',
  'plannotator.list',
  'work.get',
  'work.list',
  'proactive.status',
  'memory.status',
  'memory.entries',
  'memory.query',
  'recovery.status',
  'recovery.reconciliation',
  'recovery.points',
]);
export class RpcError extends Error {
  constructor(code, message, outcomeUnknown = false, status = 0) {
    super(clip(message, 2000));
    this.code = code;
    this.outcome_unknown = outcomeUnknown;
    this.status = status;
    this.auth = status === 401 || (status === 403 && (code === 'forbidden' || code === 'unauthorized'));
  }
}

export function beginWrite(view, key, method, params = {}) {
  const after = Math.max(view.cursor, view.snapshot?.cursor ?? 0);
  // A prompt has no accepted task identity until its own HTTP receipt returns.
  // The session's current/next attempt may belong to an entirely different client.
  const observedLease = view.snapshot?.attempt ?? view.activeAttempt?.lease ?? null;
  // Queue and Core fences differ. A cancellation can use a Core terminal proof
  // only when the original Task's Worker and Attempt match that Core lease.
  const lease = method === 'session.prompt' ? null : method === 'session.cancel'
    ? (observedLease && (!params.worker_id || observedLease.worker_id === params.worker_id) && (!params.attempt_id || observedLease.attempt_id === params.attempt_id) ? observedLease : null)
    : observedLease;
  const target = method === 'session.cancel' && sameLease(lease, view.activeAttempt?.lease) ? view.activeAttempt : null;
  // Stop can be clicked before any attempt is visible. Remember the local
  // pending prompt, never the session's next attempt from an arbitrary client.
  const prompts = method === 'session.cancel' && !lease ? view.writes.filter(write => write.method === 'session.prompt' && write.phase === 'pending') : [];
  const promptKey = prompts.length === 1 ? prompts[0].key : null;
  const write = { key, method, phase: 'pending', after: target ? Math.min(after, target.after) : after, lease, promptKey, approvalId: params.approval_id, terminal: method === 'session.cancel' ? target?.terminal ?? null : null };
  return { ...view, writes: [...view.writes, write] };
}

export function noteSessionError(view, error) {
  const faults = { ...view.faults };
  const after = Math.max(view.cursor, view.snapshot?.cursor ?? 0);
  if (error.code === 'session_frozen') faults.frozen = { after };
  return { ...view, faults };
}

export function finishWrite(view, key, error = null, result = null) {
  const rejected = error && error.outcome_unknown === false;
  const rejectedPrompt = rejected && view.writes.some(write => write.key === key && write.method === 'session.prompt');
  const next = noteSessionError(view, error ?? {});
  next.writes = next.writes.flatMap(write => {
    if (write.key !== key) return [write];
    if (rejected) return [];
    // cancelled:false is a definitive receipt for this no-op. It says nothing
    // about any other operation, whose own lock remains in place.
    if (!error && write.method === 'session.cancel' && result?.cancelled === false) return [];
    const receipt = !error && write.method === 'session.prompt' && result?.id === view.id && Array.isArray(result.messages) && safeCursor(result.cursor) && result.cursor > write.after ? result.cursor : null;
    return [{ ...write, phase: error ? 'uncertain' : 'settling', receipt, cancelled: !error && result?.cancelled === true }];
  });
  if (rejectedPrompt && !next.writes.some(write => write.method === 'session.prompt')) {
    next.originalTaskId = inferSnapshotTaskId(next.snapshot) || undefined;
  }
  // A successful prompt receipt is tied to this HTTP request. Its final cursor
  // can identify the exact durable terminal, regardless of delivery order.
  const prompt = next.writes.find(write => write.key === key && write.method === 'session.prompt' && safeCursor(write.receipt));
  if (prompt) next.writes = next.writes.map(write => write.method === 'session.cancel' && write.promptKey === key && prompt.receipt > write.after ? { ...write, promptReceipt: prompt.receipt } : write);
  return settleView(next);
}

function settleView(view) {
  const snapshot = view.snapshot;
  if (!snapshot) return view;
  const covered = fact => {
    const terminal = fact?.receipt ?? fact?.terminal;
    return safeCursor(terminal) && terminal > fact.after && view.cursor >= terminal && snapshot.cursor >= terminal && view.snapshotAfter >= terminal && view.snapshotEpoch >= (fact.terminalEpoch ?? 0);
  };
  const idle = stateName(snapshot) === 'idle' && !sessionRunning(snapshot) && !array(snapshot.pending_approvals).length;
  const hadPrompt = view.writes.some(write => write.method === 'session.prompt');
  const writes = view.writes
    .map(write => write.method === 'session.cancel' && write.cancelled && safeCursor(write.promptReceipt) && write.promptTerminal?.cursor === write.promptReceipt ? { ...write, terminal: write.promptReceipt, terminalEpoch: write.promptTerminal.epoch ?? 0 } : write)
    .filter(write => write.phase === 'pending' || (write.method === 'session.cancel' && write.phase === 'uncertain') || !covered(write) || (!idle && write.method !== 'approval.reply' && write.method !== 'session.prompt'));
  const ownsPrompt = writes.some(write => write.method === 'session.prompt');
  const originalTaskId = hadPrompt && !ownsPrompt ? (inferSnapshotTaskId(snapshot) || undefined) : view.originalTaskId;
  return { ...view, originalTaskId, activeAttempt: idle && covered(view.activeAttempt) ? null : view.activeAttempt, writes };
}

function consumeWriteFact(view, record) {
  const event = record.event;
  if (!object(event)) return view;
  const writes = view.writes.map(write => {
    if (record.cursor <= write.after) return write;
    const next = { ...write };
    const terminalLease = attemptTerminalLease(view.id, record);
    const isAttemptTerminal = Boolean(terminalLease);
    // Keep at most one candidate until the prompt receipt selects its cursor;
    // then pin that exact proof. An overwritten candidate can be reread below.
    if (next.method === 'session.cancel' && next.promptKey !== null && next.promptKey !== undefined && isAttemptTerminal && (!safeCursor(next.promptReceipt) || record.cursor === next.promptReceipt)) {
      next.promptTerminal = {cursor:record.cursor, lease:terminalLease, epoch:view.proofEpoch};
    }
    // An observed attempt still requires its complete original lease. Every
    // cancel also needs its own receipt; a lost cancel may still arrive later.
    const attemptTerminal = next.method === 'session.cancel' && isAttemptTerminal && sameLease(next.lease, terminalLease);
    const approvalTerminal = next.method === 'approval.reply' && event.type === 'approval_decided' && event.approval?.id === next.approvalId && event.approval?.attempt_id === next.lease?.attempt_id;
    if (attemptTerminal || approvalTerminal) next.terminal = record.cursor;
    return next;
  });
  const faults = { ...view.faults };
  let activeAttempt = view.activeAttempt;
  let originalTaskId = view.originalTaskId;
  const ownsPrompt = writes.some(write => write.method === 'session.prompt');
  if (event.type === 'attempt_started' && event.lease?.session_id === view.id && (!activeAttempt || record.cursor > activeAttempt.after)) {
    if (!ownsPrompt && !sameLease(activeAttempt?.lease, event.lease)) {
      originalTaskId = undefined;
    }
    activeAttempt = { lease:event.lease, after:record.cursor, terminal:null };
  } else if (activeAttempt && ['attempt_finished', 'attempt_recovered'].includes(event.type) && record.cursor > activeAttempt.after && sameLease(activeAttempt.lease,event.lease ?? record.attempt)) {
    activeAttempt = { ...activeAttempt, terminal:record.cursor };
  }
  return { ...view, writes, faults, activeAttempt, originalTaskId };
}

function writeNotice(view) {
  if (view?.writes.some(write => write.phase === 'uncertain')) return '连接断了，这次有没有完成还不能确定。不要再发一次，也不要重复停止。可以左边新建一段对话做别的事。这段先留着。';
  if (view?.writes.length || view?.activeAttempt) return '任务仍在处理，收到结束结果后即可继续发送。';
  return '';
}

// Only known, successful receipts may trigger historical reads. Neither a new
// attempt nor an idle snapshot identifies an unknown prompt/cancel request.
export function terminalReadCursor(view) {
  return view.writes.find(write => write.method === 'session.cancel' && write.phase !== 'uncertain' && write.promptKey !== null && write.promptKey !== undefined && safeCursor(write.promptReceipt) && write.promptReceipt > write.after && view.cursor >= write.promptReceipt && write.promptTerminal?.cursor !== write.promptReceipt)?.promptReceipt ?? null;
}

export function consumeTerminalRead(view, cursor, records) {
  if (terminalReadCursor(view) !== cursor || !Array.isArray(records) || records[0]?.cursor !== cursor) return view;
  const lease = attemptTerminalLease(view.id, records[0]);
  if (!lease) return view;
  // This read happens behind the forward event cursor. Its own epoch prevents
  // an already outstanding snapshot from covering newly recovered evidence.
  const epoch = view.proofEpoch + 1;
  return {...view, proofEpoch:epoch, writes:view.writes.map(write => write.method === 'session.cancel' && write.promptReceipt === cursor && write.promptKey !== null && write.promptKey !== undefined ? {...write, promptTerminal:{cursor, lease, epoch}} : write)};
}

function inferSnapshotTaskId(snapshot) {
  if (!sessionRunning(snapshot) && !array(snapshot?.pending_approvals).length) return null;
  const attemptId = typeof snapshot?.attempt?.attempt_id === 'string' && snapshot.attempt.attempt_id ? snapshot.attempt.attempt_id : null;
  for (const approval of array(snapshot?.pending_approvals)) {
    if (attemptId && typeof approval?.attempt_id === 'string' && approval.attempt_id && approval.attempt_id !== attemptId) continue;
    const taskId = approval?.input?.task_id ?? approval?.task_id;
    if (typeof taskId === 'string' && taskId) return taskId;
  }
  for (const call of array(snapshot?.unresolved_calls)) {
    if (call?.state === 'outcome_unknown' || call?.result?.outcome_unknown) continue;
    if (attemptId && typeof call?.attempt_id === 'string' && call.attempt_id && call.attempt_id !== attemptId) continue;
    const taskId = call?.task_id ?? call?.call?.task_id;
    if (typeof taskId === 'string' && taskId) return taskId;
  }
  for (const execution of array(snapshot?.executions).slice().reverse()) {
    if (
      ['running', 'awaiting_approval', 'waiting_approval'].includes(execution?.state) &&
      (!attemptId || !execution?.lease?.attempt_id || execution.lease.attempt_id === attemptId) &&
      typeof execution.task_id === 'string' &&
      execution.task_id
    ) {
      return execution.task_id;
    }
  }
  return null;
}

export function selectActiveTaskId(tasks, sessionId, snapshot = null) {
  const candidates = array(tasks).filter(
    task => task?.session_id === sessionId && typeof task.id === 'string' && task.id
  );
  if (!candidates.length) return null;
  const approvalTaskId = array(snapshot?.pending_approvals)
    .map(approval => approval?.input?.task_id ?? approval?.task_id)
    .find(taskId => typeof taskId === 'string' && taskId);
  if (approvalTaskId) {
    const matchedApproval = candidates.find(
      task => task.id === approvalTaskId && ['waiting_approval', 'awaiting_approval', 'running', 'queued', 'draining'].includes(task.state)
    );
    if (matchedApproval) return matchedApproval.id;
  }
  const attemptId = snapshot?.attempt?.attempt_id;
  if (typeof attemptId === 'string' && attemptId) {
    const matchedAttempt = candidates.slice().reverse().find(
      task => task.attempt_id === attemptId && ['running', 'waiting_approval', 'awaiting_approval', 'draining'].includes(task.state)
    );
    if (matchedAttempt) return matchedAttempt.id;
    return null;
  }
  for (const states of [
    ['running', 'waiting_approval', 'awaiting_approval'],
    ['queued'],
    ['draining'],
  ]) {
    const found = candidates.slice().reverse().find(task => states.includes(task.state) && (task.state !== 'queued' || task.worker_id));
    if (found) return found.id;
  }
  return null;
}

export function summarizeRestoreReport(report) {
  if (!object(report)) throw new Error('Runtime 未返回有效的恢复结果。');
  const snapshot = typeof report.snapshot === 'string' ? report.snapshot : '';
  const restored = array(report.restored).filter(p => typeof p === 'string' && p);
  const unchanged = array(report.unchanged).filter(p => typeof p === 'string' && p);
  const needsConfirmation = array(report.needs_confirmation).filter(p => typeof p === 'string' && p);
  const notInSnapshot = array(report.not_in_snapshot).filter(p => typeof p === 'string' && p);
  const undoSnapshot = typeof report.undo_snapshot === 'string' && report.undo_snapshot ? report.undo_snapshot : null;
  const complete = report.complete === true && needsConfirmation.length === 0;
  let headline;
  if (!complete) {
    headline = `恢复未完成：有 ${needsConfirmation.length} 个文件与当前内容冲突，需确认覆盖后才会写入（恢复点 ${clip(snapshot, 16)}）。`;
  } else if (notInSnapshot.length && !restored.length && !unchanged.length) {
    headline = `未恢复任何文件：所选路径均不在恢复点 ${clip(snapshot, 16)} 中。`;
  } else if (notInSnapshot.length) {
    headline = `恢复点 ${clip(snapshot, 16)} 处理完成（部分路径不在该恢复点中，未恢复）。`;
  } else if (!restored.length && unchanged.length) {
    headline = `恢复点 ${clip(snapshot, 16)} 检查完成：所选文件与恢复点一致，无需改动。`;
  } else {
    headline = `已从恢复点 ${clip(snapshot, 16)} 恢复 ${restored.length} 个文件。`;
  }
  const details = [];
  if (restored.length) details.push({ kind: 'restored', label: '已恢复', paths: restored });
  if (unchanged.length) details.push({ kind: 'unchanged', label: '与恢复点一致（未改动）', paths: unchanged });
  if (needsConfirmation.length) details.push({ kind: 'needs_confirmation', label: '未恢复（当前内容不同，需确认覆盖）', paths: needsConfirmation });
  if (notInSnapshot.length) details.push({ kind: 'not_in_snapshot', label: '未恢复（不在该恢复点中）', paths: notInSnapshot });
  return {
    snapshot,
    complete,
    headline,
    restored,
    unchanged,
    needsConfirmation,
    notInSnapshot,
    undoSnapshot,
    details,
  };
}

export function applySnapshot(view, snapshot, initial = false, requestedAfter = view.cursor, requestedEpoch = view.proofEpoch) {
  if (!object(snapshot) || snapshot.id !== view.id || !Array.isArray(snapshot.messages) || !safeCursor(snapshot.cursor) || !Array.isArray(snapshot.pending_approvals) || !Array.isArray(snapshot.executions)) {
    throw new Error('Runtime 返回的会话快照不完整；保留当前会话和事件游标。');
  }
  // Events and snapshots share one durable cursor. A delayed projection must
  // never recreate pending calls/attempts after their terminal facts were read.
  if (view.snapshot && snapshot.cursor < Math.max(view.snapshot.cursor, view.cursor)) return view;
  const faults = { ...view.faults };
  if (frozen(snapshot)) faults.frozen = { after: snapshot.cursor };
  // A Core snapshot may predate the runtime binding view. Its missing metadata
  // never clears an explicit frozen rejection, even at the same durable cursor.
  const pendingIds = new Set(snapshot.pending_approvals.map(approval => approval.id).filter(id => typeof id === 'string'));
  const inferredTaskId = inferSnapshotTaskId(snapshot);
  const ownsPrompt = array(view.writes).some(write => write.method === 'session.prompt');
  const previousLease = view.snapshot?.attempt ?? view.activeAttempt?.lease;
  const sameAttempt = object(snapshot.attempt) && sameLease(snapshot.attempt, previousLease);
  const nextTaskId = ownsPrompt
    ? (view.originalTaskId || inferredTaskId || undefined)
    : (inferredTaskId || (sameAttempt && sessionRunning(snapshot) ? view.originalTaskId : undefined));
  return settleView({
    ...view,
    faults,
    originalTaskId: nextTaskId,
    approvalQuestions: Object.fromEntries(Object.entries(view.approvalQuestions ?? {}).filter(([id]) => pendingIds.has(id))),
    activeAttempt: object(snapshot.attempt) && !sameLease(snapshot.attempt,view.activeAttempt?.lease) ? { lease:snapshot.attempt, after:snapshot.cursor, terminal:null } : view.activeAttempt,
    snapshot,
    executions: Object.fromEntries(snapshot.executions.map(execution => [execution.execution_id, execution])),
    totalMessages: snapshot.messages.length,
    snapshotAfter: Math.max(view.snapshotAfter, requestedAfter),
    snapshotEpoch: Math.max(view.snapshotEpoch, requestedEpoch),
    cursor: initial ? snapshot.cursor : view.cursor,
    // Bootstrap only when adopting this cursor; later snapshots can be ahead of
    // the event stream and must not cause its pending deltas to be applied twice.
    liveMessage: initial ? (snapshot.open_message?.role === 'assistant' ? snapshot.open_message : null) : view.liveMessage,
    liveText: initial ? (snapshot.open_message?.role === 'assistant' ? textOnly(snapshot.open_message) : '') : view.liveText,
  });
}

function declaredAgentEvent(event) {
  if (!object(event)) return {};
  if (['agent', 'agent_event'].includes(event.type) && object(event.event)) return event.event;
  return event;
}

export function consumeEvents(view, records) {
  if (!Array.isArray(records)) throw new Error('Runtime 未返回事件数组；事件游标未前进。');
  let previous = -1;
  for (const record of records) {
    if (!object(record) || !safeCursor(record.cursor) || record.cursor <= previous || (record.session_id !== undefined && record.session_id !== view.id)) {
      throw new Error('事件游标无效或顺序错误；保留原游标，等待重新连接。');
    }
    previous = record.cursor;
  }
  let next = { ...view, executions: { ...view.executions }, events: [...view.events] };
  for (const record of records) {
    if (record.cursor <= view.cursor) continue;
    const event = declaredAgentEvent(record.event ?? record);
    if (event.type !== 'message_update') next.snapshotRequiredAfter = record.cursor;
    if (record.cursor > (view.snapshot?.cursor ?? 0) && ['execution_started', 'execution_yielded', 'execution_finished'].includes(event.type)) {
      next.executions[event.execution.execution_id] = event.execution;
    } else if (record.cursor > (view.snapshot?.cursor ?? 0) && event.type === 'attempt_recovered') {
      for (const execution of event.executions) next.executions[execution.execution_id] = execution;
    }
    if (event.type === 'text_delta' && typeof event.delta === 'string') {
      next.liveText = `${next.liveText}${event.delta}`;
    } else if (event.type === 'message_update') {
      const update = event.event;
      if (!object(update)) throw new Error('流式消息缺少更新记录。');
      if (update.type === 'usage_update') {
        if (!object(next.liveMessage)) throw new Error('用量更新缺少消息起点。');
        next.liveMessage = {...next.liveMessage, usage:update.usage, usage_status:update.usage_status, usage_fields:update.usage_fields, response_id:update.response_id, response_model:update.response_model};
      } else if (['text_delta', 'thinking_delta', 'tool_call_delta'].includes(update.type)) {
        const block = next.liveMessage?.content?.[update.index];
        const expected = {text_delta:'text', thinking_delta:'thinking', tool_call_delta:'toolCall'}[update.type];
        if (!Number.isSafeInteger(update.index) || update.index < 0 || block?.type !== expected || typeof update.delta !== 'string') throw new Error('流式消息块无效。');
        const changed = {...block};
        if (update.type === 'tool_call_delta') {
          changed.arguments = (typeof block.arguments === 'string' ? block.arguments : '') + update.delta;
          changed.arguments_error = 'tool argument stream is incomplete';
        } else {
          const field = update.type === 'text_delta' ? 'text' : 'thinking';
          if (typeof block[field] !== 'string') throw new Error('流式消息缺少正文。');
          changed[field] = block[field] + update.delta;
        }
        const content = next.liveMessage.content.slice();
        content[update.index] = changed;
        next.liveMessage = {...next.liveMessage, content};
        if (update.type === 'text_delta') next.liveText = textOnly(next.liveMessage);
      } else {
        const snapshot = update.partial ?? update.message;
        if (!object(snapshot) || !Array.isArray(snapshot.content)) throw new Error('流式消息缺少快照。');
        next.liveMessage = snapshot;
        next.liveText = textOnly(snapshot);
      }
    } else if (event.type === 'message_start' && event.message?.role === 'assistant') {
      next.liveMessage = event.message;
      next.liveText = '';
    } else if ((event.type === 'message_end' && event.message?.role === 'assistant') || event.type === 'agent_end') {
      next.liveMessage = null;
      next.liveText = '';
    }
    next = consumeWriteFact(next, record);
    next.events.push({ cursor: record.cursor, event: record.event ?? record });
    next.cursor = record.cursor;
  }
  next.events = next.events.slice(-MAX_EVENTS);
  return next;
}

function stateName(snapshot) {
  if (typeof snapshot?.state === 'string') return snapshot.state;
  return snapshot?.state?.type ?? snapshot?.state?.status ?? 'unknown';
}
function frozen(snapshot, view) {
  return Boolean(view?.faults.frozen || snapshot?.binding?.frozen) || /frozen|unavailable/.test(stateName(snapshot));
}
export function sessionRunning(snapshot) {
  return Boolean(snapshot?.is_running || snapshot?.attempt) || /running|awaiting_approval|waiting_approval/.test(stateName(snapshot));
}
export function sessionBlocked(snapshot, view) {
  if (!snapshot) return '请先选择会话。';
  if (writeNotice(view)) return writeNotice(view);
  if (frozen(snapshot, view)) return '这段对话停住了。可以另开一段普通对话，原来的记录还在。';
  if (!['idle', 'running', 'awaiting_approval'].includes(stateName(snapshot))) return '还在同步，稍后再发送。';
  return '';
}
export function canFork(view) {
  return Boolean(view?.snapshot && frozen(view.snapshot, view) && !view.writes.length && !view.activeAttempt && !sessionRunning(view.snapshot) && !array(view.snapshot.pending_approvals).length);
}
export function stateLabel(snapshot, view, defaultApprovalMode = 'explicit') {
  if (view?.writes.some(write => write.phase === 'uncertain')) return '连接结果待确认';
  if (array(snapshot?.pending_approvals).length) {
    const allAuto = array(snapshot.pending_approvals).every(approval => {
      const id = approval?.id ?? approval?.approval_id;
      const q = view?.approvalQuestions?.[id];
      return q ? q.approval_mode === 'model_auto' : defaultApprovalMode === 'model_auto';
    });
    return allAuto ? '自动审批中' : '等待审批';
  }
  if (view?.writes.length) return '正在处理';
  if (frozen(snapshot, view)) return '已停住';
  const name = stateName(snapshot);
  return ({ idle: '就绪', ready: '就绪', running: '运行中', awaiting_approval: defaultApprovalMode === 'model_auto' ? '自动审批中' : '等待审批', cancelled: '已取消', failed: '执行失败', completed: '已完成', unknown: '状态待确认' })[name] ?? clip(name, 60);
}

if (typeof document !== 'undefined') boot();

function boot() {
  const $ = id => document.getElementById(id);
  const ui = Object.fromEntries([
    'login-panel', 'login-form', 'login-error', 'login-button', 'username', 'password', 'account-fields', 'workspace', 'logout', 'connection-status',
    'create-form', 'session-label', 'create-button', 'refresh-sessions', 'list-status', 'session-list',
    'conversation', 'session-title', 'session-state', 'global-error', 'poll-status', 'messages', 'history-note',
    'blocked-notice', 'blocked-text', 'open-fork', 'live-section', 'live-text',
    'session-artifacts', 'artifact-status', 'artifact-items', 'artifact-preview', 'refresh-artifacts',
    'task-queue', 'task-queue-items', 'task-queue-status', 'refresh-task-queue',
    'prompt-form', 'prompt', 'prompt-status', 'send-button', 'queue-follow-up', 'cancel-button', 'fork-dialog', 'fork-form',
    'fork-label', 'fork-confirm', 'fork-error', 'fork-button', 'close-fork',
  ].map(id => [id, $(id)]));
  let authMode = 'local';
  let loginRequired = false;
  let connected = false;
  let runtimeApprovalMode = 'explicit';
  let workspaceId = null;
  let mainWorkspaceId = null;
  let workspaceBusy = false;
  const mainWorkspaceScope = () => mainWorkspaceId || workspaceId;
  let settings = { reset() {} };
  const sessionWorkspaces = new Map();
  let loginGeneration = 0;
  let selected = null;
  let sessions = [];
  let views = new Map();
  let promptPending = new Map();
  let approvalPending = new Set();
  const taskAdmissions = new Map();
  const drafts = new Map();
  const followUps = new Map();
  const submittingFollowUp = new Set();
  let selectionGeneration = 0;
  let listGeneration = 0;
  let timer;
  let pollFailures = 0;
  const controllers = new Set();
  const el = (tag, className, text) => {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined) element.textContent = text;
    return element;
  };
  const errorText = error => clip(({
    session_frozen: '这段对话停住了。可以另开一段普通对话，原来的记录还在。',
    budget_exceeded: '已用完配置的预算，本次执行已停止。',
  })[error?.code] ?? (error?.outcome_unknown ? '连接中断或请求结果待确认。页面不会自动重发，请等待事件同步。' : error?.message ?? '发生未知错误。'), 2500);
  const report = error => {
    ui['global-error'].textContent = error ? errorText(error) : '';
    ui['global-error'].hidden = !error;
  };
  const activeView = () => selected ? views.get(selected) : undefined;
  const setPromptValue = value => {
    ui['prompt'].value = String(value);
  };

  async function rpc(method, params = {}, scope = sessionWorkspaces.get(params.session_id) || workspaceId) {
    const controller = new AbortController();
    controllers.add(controller);
    try {
      const requestPayload = { method, params };
      if (scope) requestPayload.workspace_id = scope;
      const response = await fetch('/api/rpc', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(requestPayload),
        signal: controller.signal,
        cache: 'no-store',
        credentials: 'same-origin',
      });
      let payload;
      try { payload = await response.json(); } catch { throw new RpcError('invalid_response', '网关返回了无效 JSON；当前请求不会自动重发。', !readMethods.has(method), response.status); }
      if (!response.ok || payload?.error) {
        const detail = payload?.error;
        throw new RpcError(typeof detail?.code === 'string' ? detail.code : 'http_error', detail?.code === 'forbidden' && response.status === 403 ? '登录已失效，请重新输入账号密码。' : detail?.message ?? `请求失败（HTTP ${response.status}）。`, typeof detail?.outcome_unknown === 'boolean' ? detail.outcome_unknown : !readMethods.has(method), response.status);
      }
      if (!object(payload) || !Object.hasOwn(payload, 'result')) throw new RpcError('invalid_response', '网关响应缺少 result；当前请求不会自动重发。', !readMethods.has(method), response.status);
      return payload.result;
    } catch (error) {
      if (error.name === 'AbortError') throw new RpcError('transport_unknown', '请求连接已中断或超时。页面继续读取事件，消息不会自动重发。', !readMethods.has(method));
      if (error instanceof TypeError) throw new RpcError('transport_unknown', '无法连接本机网关。页面继续读取事件，消息不会自动重发。', !readMethods.has(method));
      throw error;
    } finally {
      controllers.delete(controller);
    }
  }


  function disconnect(reason = '') {
    settings.reset();
    loginGeneration++;
    selectionGeneration++;
    listGeneration++;
    clearTimeout(timer);
    for (const controller of controllers) controller.abort();
    controllers.clear();
    connected = false;
    selected = null;
    sessions = [];
    // Keep unresolved operation evidence in this page's memory across
    // reauthentication. Aborting a connection never proves execution stopped.
    views = new Map([...views].filter(([,view]) => view.writes.length || view.activeAttempt || Object.keys(view.faults).length).map(([id,view]) => [id,{ ...view, writes:view.writes.map(write => ({ ...write, phase:'uncertain' })) }]));
    promptPending = new Map();
    approvalPending = new Set();
    taskAdmissions.clear();
    ui['workspace'].hidden = true;
    ui['logout'].hidden = true;
    ui['login-panel'].hidden = false;
    if (authMode === 'lan') { loginRequired = true; ui['account-fields'].hidden = false; }
    ui['login-error'].textContent = reason;
    ui['connection-status'].textContent = '尚未连接';
    ui['password'].value = '';
    ui['messages'].replaceChildren();
    ui['artifact-items']?.replaceChildren();
    ui['artifact-preview']?.replaceChildren();
    ui['live-text'].textContent = '';
    ui['session-list'].replaceChildren();
    setPromptValue('');
    ui['fork-dialog'].close();
    ui['login-button'].focus();
  }

  function handleError(error) {
    if (error.auth) disconnect(error.message);
    else report(error);
  }

  function rememberView(view) {
    views.set(view.id, view);
    // This is only the browser's bounded rendering cache. Runtime history is
    // authoritative and unaffected by removing a cached, unselected view.
    while (views.size > 8) {
      const evict = [...views.keys()].find(id => id !== selected && !promptPending.has(id) && !views.get(id).writes.length && !views.get(id).activeAttempt && !Object.keys(views.get(id).faults).length);
      if (!evict) break;
      views.delete(evict);
      sessionWorkspaces.delete(evict);
    }
  }

  function renderSessionList() {
    const fragment = document.createDocumentFragment();
    if (!sessions.length) fragment.append(el('p', 'small muted', '暂无会话。新建一个即可开始。'));
    for (const session of sessions) {
      const button = el('button', 'session-item');
      button.type = 'button';
      button.setAttribute('aria-current', String(session.id === selected));
      button.dataset.sessionId = session.id;
      button.append(el('strong', '', clip(session.label || '未命名会话', 160)), el('span', '', stateLabel(session, views.get(session.id), runtimeApprovalMode)));
      button.addEventListener('click', () => selectSession(session.id));
      fragment.append(button);
    }
    ui['session-list'].replaceChildren(fragment);
    ui['list-status'].textContent = sessions.length === 200 ? '显示最近 200 个会话。' : `${sessions.length} 个会话`;
  }

  async function refreshSessions() {
    const generation = ++listGeneration;
    const login = loginGeneration;
    ui['refresh-sessions'].disabled = true;
    ui['list-status'].textContent = '正在读取会话…';
    try {
      const result = await rpc('session.list');
      if (login !== loginGeneration || generation !== listGeneration) return;
      if (!Array.isArray(result) || result.some(session => !object(session) || typeof session.id !== 'string')) throw new Error('Runtime 返回了无效的会话列表。');
      sessions = result.slice(-200).map(sessionSummary);
      renderSessionList();
    } catch (error) {
      if (login !== loginGeneration || generation !== listGeneration) return;
      ui['list-status'].textContent = '读取失败，可点击刷新重试。';
      handleError(error);
    } finally {
      if (generation === listGeneration) ui['refresh-sessions'].disabled = false;
    }
  }

  function sessionSummary(snapshot) {
    const { id, label, cursor, state, attempt, pending_approvals, binding } = snapshot;
    return { id, label, cursor, state, attempt, pending_approvals, binding };
  }

  function updateSessionSummary(snapshot) {
    if ((sessionWorkspaces.get(snapshot.id) || 'local') !== workspaceId) return;
    const current = views.get(snapshot.id)?.snapshot;
    if (current && current.cursor > snapshot.cursor) snapshot = current;
    const index = sessions.findIndex(session => session.id === snapshot.id);
    const summary = sessionSummary(snapshot);
    if (index >= 0) sessions[index] = summary;
    else sessions = [...sessions, summary].slice(-200);
    renderSessionList();
  }

  async function selectSession(id) {
    if (!connected || id === selected) return;
    const generation = ++selectionGeneration;
    const login = loginGeneration;
    clearTimeout(timer);
    selected = id;
    sessionWorkspaces.set(id, workspaceId);
    if (!views.has(id)) rememberView(createView(id));
    ui['poll-status'].textContent = '正在读取会话…';
    if (ui['task-queue']) ui['task-queue'].open = false;
    if (ui['session-artifacts']) {
      ui['session-artifacts'].open = false;
      ui['artifact-items'].replaceChildren();
      ui['artifact-preview'].replaceChildren();
    }
    setPromptValue(drafts.get(id)?.text ?? '');
    report(null);
    renderSessionList();
    renderView();
    try {
      const requestedAfter = views.get(id).cursor;
      const requestedEpoch = views.get(id).proofEpoch;
      const snapshot = await rpc('session.get', { session_id: id });
      if (login !== loginGeneration || generation !== selectionGeneration) return;
      const previous = views.get(id);
      rememberView(applySnapshot(previous, snapshot, !previous.snapshot, requestedAfter, requestedEpoch));
      let auxError = null;
      try { await refreshActiveTaskId(id, login); } catch (error) { if (login !== loginGeneration || generation !== selectionGeneration) return; if (error.auth) throw error; auxError = error; rememberError(id, error); }
      updateSessionSummary(snapshot);
      pollFailures = 0;
      renderView();
      if (auxError) {
        report(auxError);
        ui['poll-status'].textContent = `部分状态暂不可用：${errorText(auxError)}`;
      }
      schedulePoll(0, generation);
    } catch (error) {
      if (login !== loginGeneration || generation !== selectionGeneration) return;
      ui['poll-status'].textContent = '读取会话失败，稍后重新连接。';
      rememberError(id, error);
      handleError(error);
      schedulePoll(2000, generation);
    }
  }

  function renderView() {
    const view = activeView();
    const snapshot = view?.snapshot;
    ui['session-title'].textContent = clip(snapshot?.label || (selected ? '正在读取会话…' : '选择或创建会话'), 160);
    ui['session-state'].textContent = snapshot ? stateLabel(snapshot, view, runtimeApprovalMode) : '等待选择';
    const attention = snapshot && (frozen(snapshot, view) || view?.writes.some(write => write.phase === 'uncertain') || !['idle', 'running', 'awaiting_approval'].includes(stateName(snapshot)));
    ui['blocked-text'].textContent = attention ? sessionBlocked(snapshot, view) : '';
    ui['blocked-notice'].hidden = !attention;
    ui['open-fork'].hidden = !snapshot || !frozen(snapshot, view);
    ui['open-fork'].disabled = !canFork(view);
    ui.conversation.dataset.cursor = String(view?.cursor ?? 0);
    ui.conversation.dataset.eventCursors = array(view?.events).map(record => record.cursor).join(',');
    renderMessages(view);
    renderComposer();
  }

  function renderMessages(view) {
    const fragment = document.createDocumentFragment();
    const messages = array(view?.snapshot?.messages).filter(message => message.role !== 'system').slice(-(view?.historyVisible ?? MAX_MESSAGES));
    const pendingApprovals = array(view?.snapshot?.pending_approvals);
    if (!messages.length && !pendingApprovals.length) {
      const empty = el('div', 'empty');
      empty.append(el('h2', '', view?.snapshot ? '从一条消息开始' : '开始对话'), el('p', '', view?.snapshot ? '在下面说你要做的事。' : '左边选一段已有的，或新建一段。'));
      fragment.append(empty);
    }
    const expanded = new Set(view?.expandedRecords ?? []);
    const detailsFor = (key, label, text) => {
      const details = el('details', 'record-details');
      details.open = expanded.has(key);
      const content = el('pre', '');
      let loaded = false;
      const load = () => { if (details.open && !loaded) { content.textContent = typeof text === 'function' ? text() : text; loaded = true; } };
      details.append(el('summary', '', label), content);
      load();
      details.addEventListener('toggle', () => {
        load();
        if (!details.isConnected) return;
        const current = views.get(view.id);
        if (!current) return;
        const keys = new Set(current.expandedRecords ?? []);
        if (details.open) keys.add(key);
        else keys.delete(key);
        rememberView({ ...current, expandedRecords: [...keys] });
      });
      return details;
    };
    for (const [index, message] of messages.entries()) {
      const role = ['user', 'assistant', 'system', 'toolResult', 'custom'].includes(message.role) ? message.role : 'custom';
      const card = el('article', `message ${role}`);
      const presentation = messagePresentation(message, view.executions[message.structured_content?.cell_id]);
      const key = `${message.tool_call_id ?? message.timestamp ?? index}/${index}`;
      card.append(el('div', 'role', ({ user: '你', assistant: '助手', system: '系统', toolResult: '工具结果', custom: '宿主记录' })[role]));
      if (presentation.kind === 'text') card.append(el('div', 'body', presentation.text));
      else {
        card.classList.add('operation-record');
        if (presentation.kind === 'tool_call') {
          if (presentation.text) card.append(el('div', 'body', presentation.text));
          for (const [callIndex, call] of presentation.calls.entries()) {
            const title = call.name === 'execute_code' ? call.arguments?.action === 'wait' ? '等待任务结果' : '运行任务' : `使用工具：${clip(call.name, 100)}`;
            card.append(detailsFor(`${key}/${callIndex}`, title, () => recordText(call)));
          }
        } else {
          card.append(el('strong', `result-title ${presentation.status ?? ''}`, presentation.title));
          if (presentation.text) card.append(el('p', 'result-description', presentation.text));
          for (const value of presentation.values ?? []) {
            card.append(el('div', 'output-label', value.label), el('div', 'body output-value', value.text));
          }
          card.append(detailsFor(key, '查看完整执行记录', () => presentation.raw));
        }
      }
      if (message.error_message) card.append(el('p', 'effect-warning', clip(message.error_message, 2000)));
      fragment.append(card);
    }
    for (const approval of pendingApprovals) {
      const approvalId = approval.id ?? approval.approval_id;
      const question = view?.approvalQuestions?.[approvalId];
      const isModelAuto = question ? question.approval_mode === 'model_auto' : runtimeApprovalMode === 'model_auto';
      const card = el('article', 'message system approval-question');
      card.append(el('div', 'role', isModelAuto ? '自动审批' : '询问'));
      if (!question) {
        card.append(el('div', 'body', isModelAuto ? '模型正在自动评估授权…' : '正在读取这条询问的完整选项…'));
        fragment.append(card);
        continue;
      }
      card.append(el('div', 'body', typeof question.text === 'string' ? question.text : ''));
      if (!isModelAuto) {
        const actions = el('div', 'approval-actions');
        for (const option of array(question.options)) {
          if (typeof option !== 'string' || !option) continue;
          const button = el('button', option === '拒绝' ? 'secondary deny' : option === '本任务允许' ? 'secondary' : '', approvalPending.has(`${view.id}/${approvalId}`) ? '正在记录…' : option);
          button.type = 'button';
          button.disabled = approvalPending.has(`${view.id}/${approvalId}`) || frozen(view?.snapshot, view) || view?.writes.some(write => write.phase === 'uncertain');
          button.addEventListener('click', () => replyApproval(view.id, approvalId, option));
          actions.append(button);
        }
        card.append(actions);
      }
      fragment.append(card);
    }
    ui['messages'].replaceChildren(fragment);
    const total = view?.totalMessages ?? 0;
    ui['history-note'].replaceChildren();
    const visible = view?.historyVisible ?? MAX_MESSAGES;
    if (total > visible) {
      const earlier = el('button', 'quiet', `加载更早消息（当前 ${Math.min(visible, total)} / ${total}）`);
      earlier.type = 'button';
      earlier.addEventListener('click', () => {
        const current = views.get(view.id);
        if (current) { rememberView({ ...current, historyVisible: visible + MAX_MESSAGES }); renderView(); }
      });
      ui['history-note'].append(earlier);
    } else ui['history-note'].textContent = messages.length ? `${messages.length} 条消息` : '';
    ui['live-text'].textContent = view?.liveText ?? '';
    ui['live-section'].hidden = !view?.liveText;
  }

  async function refreshTaskQueue() {
    const id = selected;
    const generation = selectionGeneration;
    const scope = sessionWorkspaces.get(id) || workspaceId;
    if (!id || !ui['task-queue']?.open) return;
    const status = ui['task-queue-status'];
    const list = ui['task-queue-items'];
    status.textContent = '正在读取队列…';
    try {
      const tasks = await rpc('session.tasks', {session_id: id}, scope);
      if (generation !== selectionGeneration) return;
      if (!Array.isArray(tasks)) throw new Error('任务队列数据不完整。');
      const queued = tasks.filter(task => task.state === 'queued');
      const rows = queued.map(task => {
        const label = clip(task.input.text, 120);
        const row = el('li', '', label + ' ');
        const cancel = el('button', 'quiet', '移除');
        cancel.type = 'button';
        cancel.setAttribute('aria-label', '移除排队任务 ' + label);
        cancel.addEventListener('click', async () => {
          try {
            const target = {session_id: id, task_id: task.id, worker_id: task.worker_id ?? null, attempt_id: task.attempt_id ?? null, fence: task.fence};
            await rpc('session.cancel', target, scope);
            if (generation === selectionGeneration) await refreshTaskQueue();
          } catch (error) {
            if (generation === selectionGeneration) status.textContent = errorText(error);
          }
        });
        row.append(cancel);
        return row;
      });
      list.replaceChildren(...rows);
      status.textContent = queued.length ? `${queued.length} 项排队中` : '没有待执行的任务。';
    } catch (error) {
      if (generation === selectionGeneration) status.textContent = errorText(error);
    }
  }

  ui['task-queue']?.addEventListener('toggle', refreshTaskQueue);
  ui['refresh-task-queue']?.addEventListener('click', refreshTaskQueue);

  let artifactRequest = 0;
  async function refreshArtifacts() {
    const id = selected;
    const generation = selectionGeneration;
    const scope = sessionWorkspaces.get(id) || workspaceId;
    if (!id || !ui['session-artifacts']?.open) return;
    const status = ui['artifact-status'];
    status.textContent = '正在读取产物…';
    try {
      const artifacts = await rpc('artifacts.list', { session_id: id, summary: true }, scope);
      if (generation !== selectionGeneration) return;
      if (!Array.isArray(artifacts)) throw new Error('产物列表不完整。');
      ui['artifact-items'].replaceChildren(...artifacts.map(artifact => {
        const row = el('li');
        const open = el('button', 'quiet', `${artifact.title} · 版本 ${artifact.content_version}`);
        open.type = 'button';
        open.addEventListener('click', async () => {
          const request = ++artifactRequest;
          const current = () => generation === selectionGeneration && request === artifactRequest;
          try {
            const preview = await rpc('artifacts.preview', { artifact_id: artifact.artifact_id }, scope);
            if (!current()) return;
            if (preview?.session_id !== id || typeof preview.html !== 'string') throw new Error('产物预览不完整。');
            const frame = el('iframe', 'artifact-frame');
            frame.title = preview.title;
            frame.setAttribute('sandbox', '');
            frame.srcdoc = `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:">${preview.html}`;
            ui['artifact-preview'].replaceChildren(frame);
            status.textContent = `已打开 ${preview.title} · 版本 ${preview.content_version}`;
          } catch (error) {
            if (current()) status.textContent = errorText(error);
          }
        });
        row.append(open);
        return row;
      }));
      status.textContent = artifacts.length ? '' : '这段会话还没有产物。';
    } catch (error) {
      if (generation === selectionGeneration) status.textContent = errorText(error);
    }
  }
  ui['session-artifacts']?.addEventListener('toggle', refreshArtifacts);
  ui['refresh-artifacts']?.addEventListener('click', refreshArtifacts);

  function renderComposer() {
    const view = activeView();
    const snapshot = view?.snapshot;
    if (ui['task-queue']) ui['task-queue'].hidden = !snapshot;
    if (ui['session-artifacts']) ui['session-artifacts'].hidden = !snapshot;
    const pending = promptPending.has(selected);
    const busy = pending || sessionRunning(snapshot);
    const blocked = sessionBlocked(snapshot, view);
    const decisionBusy = [...approvalPending].some(key => key.startsWith(`${selected}/`));
    const allAutoApprovals = array(snapshot?.pending_approvals).length > 0 && array(snapshot.pending_approvals).every(approval => {
      const id = approval?.id ?? approval?.approval_id;
      const q = view?.approvalQuestions?.[id];
      return q ? q.approval_mode === 'model_auto' : runtimeApprovalMode === 'model_auto';
    });
    const followUp = busy || Boolean(blocked) || followUps.has(selected);
    const pendingDraft = promptPending.get(selected);
    const sameSubmission = pendingDraft && pendingDraft.text === ui['prompt'].value && pendingDraft.draft === drafts.get(selected);
    ui['prompt'].disabled = !snapshot;
    ui['send-button'].disabled = !snapshot || Boolean(blocked) || busy || !ui['prompt'].value.trim();
    ui['send-button'].textContent = pending ? '等待完成…' : '发送';
    if (ui['queue-follow-up']) {
      ui['queue-follow-up'].hidden = !followUp;
      ui['queue-follow-up'].disabled = !snapshot || sameSubmission || submittingFollowUp.has(selected) || !ui['prompt'].value.trim();
    }
    ui['cancel-button'].disabled = !snapshot || (!taskAdmissions.has(selected) && !view?.originalTaskId && !busy && !array(snapshot.pending_approvals).length) || (!busy && !view.writes.length && !view.activeAttempt && !array(snapshot.pending_approvals).length) || ui['cancel-button'].dataset.pending === 'true';
    ui['prompt-status'].textContent = array(snapshot?.pending_approvals).length && !view?.writes.some(write => write.phase === 'uncertain') ? (allAutoApprovals ? '模型正在自动评估授权，无需人工操作；也可以停止运行。' : '请在对话中的询问里选择选项，也可以停止运行。') : blocked || (busy ? '正在处理，可停止运行。' : decisionBusy ? '正在记录审批决定…' : 'Enter 换行，Ctrl / ⌘ + Enter 发送。');
  }

  async function refreshActiveTaskId(id, login, forceExternal = false) {
    const view = views.get(id);
    const snapshot = view?.snapshot;
    const ownsPrompt = promptPending.has(id) || taskAdmissions.has(id) || array(view?.writes).some(write => write.method === 'session.prompt');
    if (!snapshot || ownsPrompt) return view?.originalTaskId ?? null;
    if (!forceExternal && view.originalTaskId) return view.originalTaskId;
    if (!forceExternal && !sessionRunning(snapshot) && !array(snapshot.pending_approvals).length) return null;
    const scope = sessionWorkspaces.get(id) || workspaceId;
    const tasks = await rpc('session.tasks', { session_id: id }, scope);
    if (login !== loginGeneration) return null;
    const current = views.get(id);
    const activeId = selectActiveTaskId(tasks, id, current?.snapshot ?? snapshot);
    if (current && !promptPending.has(id) && !taskAdmissions.has(id) && !array(current.writes).some(write => write.method === 'session.prompt')) {
      rememberView({ ...current, originalTaskId: activeId || undefined });
    }
    return activeId;
  }

  // Read the exact question and three options that the runtime prepared for
  // each pending approval. This is a read-only call; it never decides.
  async function refreshApprovalQuestions(id, login) {
    const view = views.get(id);
    if (!view?.snapshot) return;
    const pending = view.snapshot.pending_approvals.map(approval => approval.id).filter(id => typeof id === 'string');
    const questions = {};
    for (const approvalId of pending) {
      const cached = view.approvalQuestions?.[approvalId];
      if (cached) { questions[approvalId] = cached; continue; }
      try {
        const question = await rpc('approval.question', { session_id: id, approval_id: approvalId });
        if (login !== loginGeneration) return;
        const isModelAuto = question?.approval_mode === 'model_auto';
        const validOptions = Array.isArray(question?.options) && question.options.length === (isModelAuto ? 0 : 3);
        if (question?.session_id !== id || question?.approval_id !== approvalId || !validOptions) {
          throw new RpcError('invalid_response', 'Runtime 返回的审批询问不完整；页面未显示选项。', false);
        }
        questions[approvalId] = question;
      } catch (error) {
        if (error?.code === 'stale_approval') continue;
        throw error;
      }
    }
    if (login !== loginGeneration) return;
    const current = views.get(id);
    if (current) rememberView({ ...current, approvalQuestions: questions });
  }

  async function replyApproval(id, approvalId, reply) {
    const key = `${id}/${approvalId}`;
    if (approvalPending.has(key)) return;
    const login = loginGeneration;
    approvalPending.add(key);
    report(null);
    renderView();
    try {
      await writeRpc(id, 'approval.reply', { session_id: id, approval_id: approvalId, reply });
      if (login !== loginGeneration) return;
      await freshSnapshot(id, login);
    } catch (error) {
      if (login === loginGeneration) handleError(error);
    } finally {
      if (login === loginGeneration) {
        approvalPending.delete(key);
        renderView();
      }
    }
  }

  function schedulePoll(delay, generation = selectionGeneration) {
    clearTimeout(timer);
    if (connected && selected && generation === selectionGeneration) timer = setTimeout(() => poll(generation), delay);
  }

  async function poll(generation) {
    const id = selected;
    const login = loginGeneration;
    if (!id || !connected || generation !== selectionGeneration) return;
    let delay = 1000;
    try {
      let view = views.get(id);
      if (!view.snapshot) {
        const requestedAfter = views.get(id).cursor;
        const requestedEpoch = views.get(id).proofEpoch;
        const snapshot = await rpc('session.get', { session_id: id });
        if (login !== loginGeneration || generation !== selectionGeneration) return;
        view = applySnapshot(views.get(id), snapshot, true, requestedAfter, requestedEpoch);
        rememberView(view);
      }
      const records = await rpc('session.events', { session_id: id, after: view.cursor, limit: 100 });
      if (login !== loginGeneration || generation !== selectionGeneration) return;
      view = consumeEvents(views.get(id), records);
      // Commit and render successfully consumed events before a later snapshot
      // request can fail. Reconnection then starts at exactly this cursor.
      rememberView(view);
      if (ui['live-text'].textContent !== view.liveText) ui['live-text'].textContent = view.liveText;
      ui['live-section'].hidden = !view.liveText;
      const terminalCursor = terminalReadCursor(view);
      if (terminalCursor !== null) {
        const proof = await rpc('session.events', {session_id:id, after:terminalCursor - 1, limit:1});
        if (login !== loginGeneration || generation !== selectionGeneration) return;
        rememberView(consumeTerminalRead(views.get(id), terminalCursor, proof));
      }
      view = views.get(id);
      const requestedAfter = view.cursor;
      const requestedEpoch = view.proofEpoch;
      // Text, thinking and tool argument fragments are already rendered from
      // the log. Fetch a snapshot for committed state changes and recovery.
      if (records.length && !pollFailures && view.snapshot.cursor < view.cursor &&
          view.snapshot.cursor >= view.snapshotRequiredAfter && view.snapshotEpoch === requestedEpoch) {
        if (records.length === 100) delay = 0;
        return;
      }
      const canReuseSnapshot = view.snapshot?.cursor === view.cursor && !view.writes.length &&
        !view.activeAttempt && !Object.keys(view.faults).length && !sessionRunning(view.snapshot);
      const refreshed = await rpc('session.get', {
        session_id: id,
        ...(canReuseSnapshot ? { if_cursor: view.cursor } : {})
      });
      if (login !== loginGeneration || generation !== selectionGeneration) return;
      const snapshot = refreshed === null ? view.snapshot : refreshed;
      if (refreshed !== null) rememberView(applySnapshot(views.get(id), refreshed, false, requestedAfter, requestedEpoch));
      let auxError = null;
      try { await refreshActiveTaskId(id, login); } catch (error) { if (login !== loginGeneration || generation !== selectionGeneration) return; if (error.auth) throw error; auxError = error; rememberError(id, error); }
      try { await refreshApprovalQuestions(id, login); } catch (error) { if (login !== loginGeneration || generation !== selectionGeneration) return; if (error.auth) throw error; auxError = auxError ?? error; rememberError(id, error); }
      if (login !== loginGeneration || generation !== selectionGeneration) return;
      if (refreshed !== null) {
        updateSessionSummary(snapshot);
        renderView();
      }
      pollFailures = 0;
      ui['connection-status'].textContent = '已连接本机';
      if (auxError) {
        report(auxError);
        ui['poll-status'].textContent = `部分状态暂不可用：${errorText(auxError)}`;
      } else {
        ui['poll-status'].textContent = '已同步';
      }
      if (records.length === 100) delay = 0;
    } catch (error) {
      if (login !== loginGeneration || generation !== selectionGeneration) return;
      if (error.auth) { disconnect(error.message); return; }
      rememberError(id, error);
      pollFailures++;
      delay = Math.min(1000 * 2 ** Math.min(pollFailures, 4), 15000);
      ui['connection-status'].textContent = '正在重连';
      ui['poll-status'].textContent = `事件读取失败，${Math.round(delay / 1000)} 秒后续读；未重发消息。${errorText(error)}`;
    } finally {
      if (login === loginGeneration && generation === selectionGeneration) schedulePoll(delay, generation);
    }
  }

  let writeSequence = 0;
  async function writeRpc(id, method, params) {
    const scope = sessionWorkspaces.get(id) || workspaceId;
    sessionWorkspaces.set(id, scope);
    const login = loginGeneration;
    const key = ++writeSequence;
    const view = views.get(id);
    if (view) rememberView(beginWrite(view, key, method, params));
    if (id === selected) renderView();
    let currentAdmission = null;
    try {
      let result;
      if (method === 'session.prompt') {
        // Admission returns our Task ID before execution starts. Never select a
        // Session's current task when a delayed stop action eventually runs.
        const admission = rpc('task.enqueue', params, scope).then(task => {
          if (task?.session_id !== id || typeof task.id !== 'string' || !task.id) throw new RpcError('invalid_response', '任务入队未返回原任务编号，请由宿主核对。', true);
          if (login === loginGeneration && views.has(id)) rememberView({ ...views.get(id), originalTaskId: task.id });
          return task.id;
        });
        currentAdmission = admission;
        taskAdmissions.set(id, admission);
        if (id === selected) renderComposer();
        const taskId = await admission;
        result = await rpc('task.resume', { task_id: taskId }, scope);
      } else result = await rpc(method, params, scope);
      if (login === loginGeneration && views.has(id)) rememberView(finishWrite(views.get(id), key, null, result));
      return result;
    } catch (error) {
      if (login === loginGeneration && views.has(id)) rememberView(finishWrite(views.get(id), key, error));
      throw error;
    } finally {
      if (currentAdmission && taskAdmissions.get(id) === currentAdmission) taskAdmissions.delete(id);
      if (login === loginGeneration && id === selected) renderView();
    }
  }

  function rememberError(id, error) {
    const view = views.get(id);
    if (view) rememberView(noteSessionError(view, error));
    if (id === selected) renderView();
  }

  async function freshSnapshot(id, login) {
    const requestedAfter = views.get(id)?.cursor ?? 0;
    const requestedEpoch = views.get(id)?.proofEpoch ?? 0;
    const snapshot = await rpc('session.get', { session_id: id });
    if (login !== loginGeneration) return;
    const previous = views.get(id);
    if (previous) rememberView(applySnapshot(previous, snapshot, false, requestedAfter, requestedEpoch));
    let auxError = null;
    try { await refreshActiveTaskId(id, login); } catch (error) { if (login !== loginGeneration) return; if (error.auth) throw error; auxError = error; rememberError(id, error); }
    try { await refreshApprovalQuestions(id, login); } catch (error) { if (login !== loginGeneration) return; if (error.auth) throw error; auxError = auxError ?? error; rememberError(id, error); }
    if (login !== loginGeneration) return;
    updateSessionSummary(snapshot);
    if (id === selected) {
      renderView();
      if (auxError) {
        report(auxError);
        ui['poll-status'].textContent = `部分状态暂不可用：${errorText(auxError)}`;
      }
    }
  }

  async function activateLaunchWorkspace() {
    const requested = workspaceIdFromHash(window.location?.hash ?? '');
    if (!requested) return;
    const catalog = await rpc('settings.workspaces.list', {}, null);
    const entry = array(catalog?.workspaces).find(workspace => workspace?.id === requested);
    if (!entry) throw new RpcError('workspace_unavailable', '链接指定的工作区尚未登记。请先运行 cayw init --workspace DIR。', false);
    if (entry.state === 'unavailable') throw new RpcError('workspace_unavailable', '链接指定的工作区目录当前不可用。', false);
    const opened = await rpc('settings.workspaces.open', { id: requested }, null);
    if (opened?.id !== requested) throw new RpcError('invalid_response', 'Client 返回了不同的工作区；页面未继续。', true);
    workspaceId = requested;
  }

  async function connect() {
    if (ui['login-button'].disabled) return;
    ui['password'].value = '';
    ui['login-button'].disabled = true;
    ui['login-button'].textContent = '进入中…';
    ui['login-error'].textContent = '';
    const login = ++loginGeneration;
    try {
      await activateLaunchWorkspace();
      const health = await rpc('runtime.health');
      if (login !== loginGeneration) return;
      if (health?.status !== 'ok' || health?.protocol_version !== CORE_PROTOCOL_VERSION) throw new Error('本机运行服务的版本不匹配，请同步更新运行服务和网页界面。');
      runtimeApprovalMode = health?.approval_mode === 'model_auto' ? 'model_auto' : 'explicit';
      connected = true;
      ui['login-panel'].hidden = true;
      ui['workspace'].hidden = false;
      ui['logout'].hidden = !loginRequired;
      ui['connection-status'].textContent = '已连接本机';
      report(null);
      renderView();
      await refreshWorkspaceCatalog();
      await refreshSessions();
      if (login === loginGeneration && sessions.length) await selectSession(sessions[0].id);
      else ui['session-label'].focus();
    } catch (error) {
      if (login !== loginGeneration) return;
      disconnect(errorText(error));
    } finally {
      ui['login-button'].disabled = false;
      ui['login-button'].textContent = '进入';
    }
  }
  ui['login-form'].addEventListener('submit', async event => {
    event.preventDefault();
    if (ui['login-button'].disabled) return;
    if (loginRequired) {
      ui['login-button'].disabled = true;
      try {
        const response = await fetch('/api/auth/login', {
          method:'POST', credentials:'same-origin', cache:'no-store',
          headers:{'Content-Type':'application/json'},
          body:JSON.stringify({username:ui.username.value,password:ui.password.value}),
        });
        if (!response.ok) { const payload = await response.json(); throw new Error(payload.error?.message || '登录失败'); }
      } catch (error) { ui['login-error'].textContent = error.message; return; }
      finally { ui.password.value = ''; ui['login-button'].disabled = false; }
    }
    return connect();
  });
  async function initializeAccess() {
    ui['login-button'].disabled = true;
    try {
      const response = await fetch('/api/auth/status', {credentials:'same-origin',cache:'no-store'});
      if (!response.ok) throw new Error('无法读取连接模式，请刷新重试。');
      const access = await response.json();
      if (!['local','lan'].includes(access.mode)) throw new Error('网页与网关版本不匹配。');
      authMode = access.mode; loginRequired = Boolean(access.login_required); ui['account-fields'].hidden = !loginRequired;
      renderLanNotice(access.lan_active === true && access.lan_login_enabled !== true);
      ui['login-button'].disabled = false;
      if (access.authenticated || authMode === 'local') await connect();
      else { ui['login-error'].textContent = '请使用账号密码登录'; ui.username.focus(); }
    } catch (error) { ui['login-error'].textContent = error.message; }
    finally { ui['login-button'].disabled = false; }
  }
  initializeAccess();

  async function refreshWorkspaceCatalog() {
    const status = $('workspace-switch-status');
    try {
      const catalog = await rpc('settings.workspaces.list');
      if (!Array.isArray(catalog?.workspaces)) throw new Error('当前 Client 未提供工作区目录。');
      mainWorkspaceId = catalog.workspaces.find(workspace => workspace.main_session)?.id || null;
      const active = await rpc('settings.workspace');
      const current = catalog.workspaces.find(workspace => workspace.path === active?.workspace);
      if (!current) throw new Error('当前工作区不在 Client 的登记目录中。');
      workspaceId = current.id;
      const picker = $('workspace-choice'); picker.replaceChildren();
      for (const workspace of catalog.workspaces) {
        const option = el('option', '', `${workspace.path}${workspace.state === 'unavailable' ? '（目录不可用）' : ''}`);
        option.value = workspace.id; option.disabled = workspace.state === 'unavailable'; picker.append(option);
      }
      picker.value = workspaceId;
      $('switch-workspace').disabled = workspaceBusy;
      status.textContent = '';
    } catch (error) { $('switch-workspace').disabled = true; status.textContent = `工作区列表没读到。${error.message || String(error)}`; }
  }
  async function switchWorkspace() {
    if (workspaceBusy || !connected) return;
    settings.reset();
    workspaceBusy = true; $('switch-workspace').disabled = true;
    const login = loginGeneration;
    const status = $('workspace-switch-status');
    status.textContent = '正在打开…';
    try {
      const target = await rpc('settings.workspaces.open', {id:$('workspace-choice').value});
      if (login !== loginGeneration) return;
      if (typeof target?.id !== 'string' || typeof target?.path !== 'string') throw new Error('工作区打开回执无效，请刷新状态。');
      if (target.id !== workspaceId) {
        // In-flight enqueue/resume keeps its captured workspace and transport.
        // Do not disconnect, abort controllers, or cancel another root's task.
        settings.reset();
        workspaceId = target.id;
        ++selectionGeneration; ++listGeneration; clearTimeout(timer);
        selected = null; sessions = [];
        ui['fork-dialog'].close(); setPromptValue('');        report(null); renderSessionList(); renderView();
await refreshSessions();
        if (sessions.length) await selectSession(sessions[0].id);
      }
      $('workspace-choice').value = target.id;
      window.history?.replaceState(null, '', `#workspace=${target.id}`);
      status.textContent = target.path;
    } catch (error) { status.textContent = errorText(error); }
    finally { workspaceBusy = false; $('switch-workspace').disabled = false; }
  }
  $('switch-workspace').addEventListener('click', switchWorkspace);
  $('refresh-workspaces').addEventListener('click', refreshWorkspaceCatalog);

  ui['logout'].addEventListener('click', async () => {
    settings.reset();
    try {
      const response = await fetch('/api/auth/logout', { method:'POST', credentials:'same-origin' });
      if (!response.ok) throw new Error('无法注销本机会话，请重试。');
      disconnect('已退出，请重新登录。');
    } catch (error) { report(error); }
  });
  ui['refresh-sessions'].addEventListener('click', refreshSessions);
  ui['create-form'].addEventListener('submit', async event => {
    event.preventDefault();
    if (ui['create-button'].disabled || !ui['session-label'].value.trim()) return;
    const login = loginGeneration;
    const scope = workspaceId;
    ui['create-button'].disabled = true;
    ui['create-button'].textContent = '创建中…';
    report(null);
    try {
      const params = { label: ui['session-label'].value.trim() };
      const snapshot = await rpc('session.create', params);
      sessionWorkspaces.set(snapshot.id, scope);
      if (login !== loginGeneration || scope !== workspaceId) return;
      const view = applySnapshot(createView(snapshot.id), snapshot, true);
      rememberView(view);
      ui['session-label'].value = '';
      updateSessionSummary(snapshot);
      await selectSession(snapshot.id);
    } catch (error) {
      if (login === loginGeneration) handleError(error);
    } finally {
      ui['create-button'].disabled = false;
      ui['create-button'].textContent = '新建';
    }
  });

  ui['prompt'].addEventListener('input', () => {
    if (selected) drafts.set(selected, {text: ui['prompt'].value});
    renderComposer();
  });
  ui['prompt'].addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.isComposing && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      const followUp = ui['queue-follow-up'];
      const queued = followUps.get(selected)?.has(ui['prompt'].value.trim());
      const busy = promptPending.has(selected) || sessionRunning(activeView()?.snapshot);
      if ((busy || queued) && followUp && !followUp.hidden && !followUp.disabled) ui['prompt-form'].requestSubmit(followUp);
      else if (!ui['send-button'].disabled) ui['prompt-form'].requestSubmit();
    }
  });
  ui['prompt-form'].addEventListener('submit', async event => {
    event.preventDefault();
    const isFollowUp = event.submitter?.id === 'queue-follow-up';
    if (!selected || (isFollowUp ? ui['queue-follow-up'].disabled : ui['send-button'].disabled)) return;
    const id = selected;
    const draft = ui['prompt'].value;
    const submittedDraft = drafts.get(id);
    const clearSubmittedDraft = () => {
      if (drafts.get(id) !== submittedDraft) return;
      drafts.delete(id);
      if (id === selected && ui['prompt'].value === draft) setPromptValue('');
    };
    const text = draft.trim();
    const login = loginGeneration;
    const scope = workspaceId;
    const requestedAfter = views.get(id)?.cursor ?? 0;
    const requestedEpoch = views.get(id)?.proofEpoch ?? 0;
    // An exact option typed as the whole message answers the displayed
    // approval question. The program matches it; it never reaches the model.
    const pendingApproval = array(views.get(id)?.snapshot?.pending_approvals)[0];
    const pendingQuestion = pendingApproval ? views.get(id)?.approvalQuestions?.[pendingApproval.id ?? pendingApproval.approval_id] : null;
    const exactOption = pendingQuestion ? array(pendingQuestion.options).find(option => option === text) : null;
    if (exactOption && login === loginGeneration) {
      setPromptValue('');
      await replyApproval(id, pendingApproval.id ?? pendingApproval.approval_id, exactOption);
      return;
    }
    if (isFollowUp) {
      const submissions = followUps.get(id) ?? new Map();
      const previous = submissions.get(text);
      const bytes = crypto.getRandomValues(new Uint8Array(16));
      bytes[6] = (bytes[6] & 15) | 64;
      bytes[8] = (bytes[8] & 63) | 128;
      const key = Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
      const request = previous ?? { session_id: id, text, request_id: key.replace(/^(.{8})(.{4})(.{4})(.{4})(.{12})$/, '$1-$2-$3-$4-$5') };
      submissions.set(text, request);
      followUps.set(id, submissions);
      submittingFollowUp.add(id);
      report(null);
      renderComposer();
      try {
        const task = await rpc('task.submit_follow_up', request, scope);
        if (login !== loginGeneration) return;
        if (task?.session_id !== id || typeof task.id !== 'string') throw new RpcError('invalid_response', '未收到任务编号。', true);
        submissions.delete(text);
        if (!submissions.size) followUps.delete(id);
        clearSubmittedDraft();
        if (id === selected) {
          ui['poll-status'].textContent = `已受理任务 ${task.id}`;
          await refreshTaskQueue();
        }
      } catch (error) {
        if (error.outcome_unknown === false) submissions.delete(text);
        if (!submissions.size) followUps.delete(id);
        if (id === selected && login === loginGeneration) handleError(error);
      } finally {
        submittingFollowUp.delete(id);
        if (id === selected) renderComposer();
      }
      return;
    }
    promptPending.set(id, {text: draft, draft: submittedDraft});
    report(null);
    renderComposer();
    try {
      // Never await prompt from inside the polling loop. Approval/cancel/events
      // need separate concurrent HTTP and IPC requests while it is pending.
      const snapshot = await writeRpc(id, 'session.prompt', { session_id: id, text });
      if (login !== loginGeneration) return;
      const view = views.get(id);
      if (view) rememberView({ ...applySnapshot(view, snapshot, false, requestedAfter, requestedEpoch), liveText: '' });
      updateSessionSummary(snapshot);
      clearSubmittedDraft();
      if (id === selected) renderView();
    } catch (error) {
      if (login !== loginGeneration) return;
      if (scope === workspaceId) handleError(error);
    } finally {
      if (login === loginGeneration) {
        promptPending.delete(id);
        const current = views.get(id);
        if (current && !taskAdmissions.has(id) && !array(current.writes).some(write => write.method === 'session.prompt') && (sessionRunning(current.snapshot) || array(current.snapshot?.pending_approvals).length)) {
          refreshActiveTaskId(id, login).then(() => {
            if (login === loginGeneration && id === selected) renderComposer();
          }).catch(error => {
            if (login !== loginGeneration) return;
            rememberError(id, error);
            if (id === selected) {
              handleError(error);
              ui['poll-status'].textContent = `部分状态暂不可用：${errorText(error)}`;
            }
          });
        }
        if (id === selected) renderComposer();
      }
    }
  });

  ui['cancel-button'].addEventListener('click', async () => {
    if (ui['cancel-button'].disabled || !selected) return;
    const id = selected;
    const login = loginGeneration;
    const scope = sessionWorkspaces.get(id) || workspaceId;
    ui['cancel-button'].dataset.pending = 'true';
    ui['cancel-button'].textContent = '正在停止…';
    renderComposer();
    report(null);
    try {
      // Capture the original admission before any await when this client owns a
      // prompt. External tasks resolve the live task from session.tasks instead
      // of reusing a previous task's cached ID.
      const currentView = views.get(id);
      const admission = taskAdmissions.get(id);
      const ownsPrompt = promptPending.has(id) || Boolean(admission) || array(currentView?.writes).some(write => write.method === 'session.prompt');
      let taskId;
      if (ownsPrompt) {
        taskId = admission ? await admission : currentView?.originalTaskId;
      } else {
        taskId = await refreshActiveTaskId(id, login, true);
        if (typeof taskId !== 'string' || !taskId) {
          taskId = inferSnapshotTaskId(views.get(id)?.snapshot);
        }
      }
      if (typeof taskId !== 'string' || !taskId) throw new RpcError('invalid_params', '未保存原任务编号，请在宿主中指定原 Task 后取消。');
      const task = await rpc('task.get', { task_id: taskId }, scope);
      if (task?.id !== taskId || task.session_id !== id) throw new RpcError('invalid_response', '返回的任务与原取消目标不符。', true);
      if (login !== loginGeneration) return;
      const target = { session_id: id, task_id: taskId, worker_id: task.worker_id, attempt_id: task.attempt_id, fence: task.fence };
      const result = await writeRpc(id, 'session.cancel', target);
      if (login !== loginGeneration) return;
      ui['poll-status'].textContent = result?.cancelled ? '已请求取消；等待 Runtime 记录实际终态。' : 'Runtime 当前没有可取消的执行；继续读取真实状态。';
      await freshSnapshot(id, login);
    } catch (error) { if (login === loginGeneration) handleError(error); }
    finally {
      ui['cancel-button'].dataset.pending = 'false';
      ui['cancel-button'].textContent = '停止';
      renderComposer();
    }
  });

  ui['open-fork'].addEventListener('click', () => {
    if (!canFork(activeView())) return;
    ui['fork-label'].value = `${activeView()?.snapshot?.label ?? '对话'} · 继续`.slice(0, 160);
    ui['fork-confirm'].checked = false;
    ui['fork-error'].textContent = '';
    ui['fork-dialog'].showModal();
  });
  ui['close-fork'].addEventListener('click', () => ui['fork-dialog'].close());
  ui['fork-form'].addEventListener('submit', async event => {
    event.preventDefault();
    if (!ui['fork-confirm'].checked || ui['fork-button'].disabled || !selected || !canFork(activeView())) return;
    const id = selected;
    const login = loginGeneration;
    const scope = sessionWorkspaces.get(id) || workspaceId;
    ui['fork-button'].disabled = true;
    ui['fork-button'].textContent = '正在创建…';
    ui['fork-error'].textContent = '';
    try {
      const snapshot = await writeRpc(id, 'session.fork', { session_id: id, label: ui['fork-label'].value.trim(), confirm: true, mode: 'chat' });
      // Validate the operation receipt before clearing its source lock. A
      // malformed success may already have created a session and cannot retry.
      if (typeof snapshot?.id !== 'string' || !snapshot.id || snapshot.id === id) throw new RpcError('invalid_response', '新对话结果不完整。不要再另开一次。', true);
      const forkView = applySnapshot(createView(snapshot.id), snapshot, true);
      const source = views.get(id);
      if (source && login === loginGeneration) rememberView({ ...source, writes: source.writes.filter(write => write.method !== 'session.fork') });
      if (login !== loginGeneration) return;
      sessionWorkspaces.set(snapshot.id, scope);
      rememberView(forkView);
      if (scope !== workspaceId) return;
      updateSessionSummary(snapshot);
      ui['fork-dialog'].close();
      await selectSession(snapshot.id);
    } catch (error) {
      if (login === loginGeneration) {
        const source = views.get(id);
        if (source) rememberView({ ...source, writes:source.writes.map(write => write.method === 'session.fork' ? { ...write,phase:'uncertain' } : write) });
        if (error.auth) { disconnect(error.message); return; }
        if (scope !== workspaceId) return;
        renderView();
        ui['fork-error'].textContent = errorText(error);
      }
    } finally {
      if (login !== loginGeneration) return;
      ui['fork-button'].disabled = false;
      ui['fork-button'].textContent = '另开';
    }
  });

  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && connected && selected) schedulePoll(0);
  });
  window.addEventListener('online', () => { if (connected && selected) schedulePoll(0); });
  window.addEventListener('pagehide', () => { ui.password.value = ''; for (const controller of controllers) controller.abort(); });
}
