const CORE_PROTOCOL_VERSION = 3;
const MAX_MESSAGES = 120;
const MAX_EVENTS = 120;
const MAX_TEXT = 64000;
const MAX_RECORD = 12000;
const clip = (value, limit = MAX_TEXT) => {
  const text = String(value ?? '');
  return text.length > limit ? `${text.slice(0, limit)}\n…内容过长，已截断显示。` : text;
};
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const safeCursor = value => Number.isSafeInteger(value) && value >= 0;
const array = value => Array.isArray(value) ? value : [];
const recordText = value => clip(JSON.stringify(value, null, 2) ?? '', MAX_RECORD);

function renderLanNotice(open) {
  const target = document.getElementById('network-notice');
  if (!target) return;
  target.replaceChildren();
  if (open) { const note = document.createElement('p'); note.className = 'notice'; note.textContent = '局域网访问已开放，未配置账号，网络内可直接访问。请在运行 CAYW 的电脑上打开本机管理页面，在设置中配置可选账号。'; target.append(note); }
}

function localServiceUrl(baseUrl) {
  let url;
  try { url = new URL(baseUrl); } catch { throw new Error('请填写服务启动时输出的完整本机地址。'); }
  const local = url.hostname === 'localhost' || url.hostname === '[::1]' || /^127(?:\.\d{1,3}){3}$/.test(url.hostname);
  if (!local || !['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
    throw new Error('服务地址必须是本机 HTTP 地址，不能包含令牌、查询参数或片段。');
  }
  return url.toString();
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

export function buildVisualEditorLaunchUrl({ baseUrl } = {}) {
  return localServiceUrl(baseUrl);
}

export function buildTraceDebugLaunchUrl({ baseUrl } = {}) {
  return localServiceUrl(baseUrl);
}

export function validatePlanFeedback(plan, draft) {
  if (!plan || !plan.task_id) {
    throw new Error('当前会话尚未生成计划，无法提交批注');
  }
  if (!draft?.decision) {
    throw new Error('必须选择批注决定 (decision)');
  }
  if (!Number.isSafeInteger(plan.version) || plan.version < 1 || !/^[0-9a-f]{64}$/.test(plan.content_sha256 || '')) {
    throw new Error('计划缺少有效的版本和内容摘要，请重新读取计划。');
  }
  return {
    session_id: draft.session_id,
    expected_task_id: plan.task_id,
    expected_version: plan.version,
    expected_content_sha256: plan.content_sha256,
    decision: draft.decision,
    summary: draft.summary || '批注审查意见',
    comments: draft.comments || [],
  };
}

export function messageText(message) {
  if (!object(message)) return clip(message);
  const warning = message.outcome_unknown ? '工具结果待核对；需要由宿主核对真实副作用。' : message.is_error ? '工具执行失败。' : '';
  const content = messageContent(message);
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

export function messagePresentation(message) {
  const raw = messageText(message);
  if (message?.role === 'toolResult') {
    const page = message.structured_content;
    const knownPage = message.tool_name === 'execute_code' && object(page) && ['completed', 'failed', 'cancelled', 'outcome_unknown', 'running', 'yielded'].includes(page.status) && Array.isArray(page.outputs);
    const unknown = Boolean(message.outcome_unknown || (knownPage && (page.outcome_unknown || page.status === 'outcome_unknown')));
    const status = unknown ? 'unknown' : message.is_error && page?.status !== 'cancelled' ? 'failed' : knownPage ? page.status : 'record';
    const title = ({ completed: '执行完成', failed: '执行未完成', cancelled: '已停止', outcome_unknown: '执行结果待核对', unknown: '执行结果待核对', running: '仍在执行', yielded: '等待继续', record: '工具返回了执行记录' })[status];
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
    if (unknown) detail = '请由宿主核对操作是否已发生；此结果不会自动重试。';
    else if (status === 'cancelled') detail = '任务已停止。';
    else if (status === 'failed') {
      const reason = typeof page?.error === 'string' ? page.error : '';
      detail = /permission denied/.test(reason) ? '有一项操作未获授权，任务已结束。' : /^Timeout:/.test(reason) ? '任务超过本次运行时限。' : '执行未完成，可展开查看原因。';
    }
    return { kind: 'tool_result', title, status, text: detail, values, raw };
  }
  const calls = array(message?.content).filter(part => part?.type === 'toolCall');
  if (message?.role === 'assistant' && calls.length) return { kind: 'tool_call', title: '执行工具', text: textOnly(message), calls, raw };
  if (message?.role === 'assistant' && message.provider === 'fixture' && message.model === 'offline-runtime' && raw.startsWith('Offline fixture: ')) {
    return { kind: 'record', title: '离线测试回复', text: '已收到离线测试的执行记录。', raw };
  }
  return { kind: 'text', text: raw, raw };
}

export function createView(id) {
  return { id, cursor: 0, snapshot: null, events: [], liveText: '', totalMessages: 0, snapshotAfter: 0, proofEpoch: 0, snapshotEpoch: 0, writes: [], faults: {}, activeAttempt: null };
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
  'session.run_inspector',
  'model.budget.status',
  'artifacts.list',
  'artifacts.preview',
  'artifacts.action_list',
  'artifacts.action_get',
  'plannotator.list',
]);
export class RpcError extends Error {
  constructor(code, message, outcomeUnknown = false, status = 0) {
    super(clip(message, 2000));
    this.code = code;
    this.outcome_unknown = outcomeUnknown;
    this.status = status;
    this.auth = status === 403 || status === 401 || (code === 'forbidden' && status === 403);
  }
}

export function unstartedContinuationCancelTarget(task, sessionId, taskId) {
  const pristine = object(task)
    && task.id === taskId
    && task.session_id === sessionId
    && task.state === 'queued'
    && task.worker_id === null
    && task.attempt_id === null
    && task.dispatches === 0
    && task.holds_slot === false
    && task.cancel_requested === false
    && safeCursor(task.fence);
  if (!pristine) {
    throw new RpcError(
      'budget_continuation_cleanup_unknown',
      '新任务已不再是从未领取的 queued 状态；无法证明模型请求未开始。',
      true
    );
  }
  return {
    session_id: task.session_id,
    task_id: task.id,
    worker_id: task.worker_id,
    attempt_id: task.attempt_id,
    fence: task.fence,
  };
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
  if ((error.code === 'needs_reconciliation' || error.code === 'needs_reconciliation_outcome_unknown' || error.code === 'tool_outcome_unknown') && !faults.reconciliation) faults.reconciliation = { after, callIds: [], terminal: null };
  return { ...view, faults };
}

export function finishWrite(view, key, error = null, result = null) {
  const rejected = error && (error.outcome_unknown === false || error.code === 'needs_reconciliation_outcome_unknown');
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
  const idle = stateName(snapshot) === 'idle' && !sessionRunning(snapshot) && !unresolved(snapshot) && !array(snapshot.pending_approvals).length;
  const faults = { ...view.faults };
  if (idle && covered(faults.reconciliation)) delete faults.reconciliation;
  const writes = view.writes.map(write => write.method === 'session.cancel' && write.cancelled && safeCursor(write.promptReceipt) && write.promptTerminal?.cursor === write.promptReceipt ? { ...write, terminal: write.promptReceipt, terminalEpoch: write.promptTerminal.epoch ?? 0 } : write);
  return { ...view, faults, activeAttempt: idle && covered(view.activeAttempt) ? null : view.activeAttempt, writes: writes.filter(write => write.phase === 'pending' || (write.method === 'session.cancel' && write.phase === 'uncertain') || !covered(write) || (!idle && write.method !== 'approval.decide')) };
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
    const approvalTerminal = next.method === 'approval.decide' && event.type === 'approval_decided' && event.approval?.id === next.approvalId && event.approval?.attempt_id === next.lease?.attempt_id;
    if (attemptTerminal || approvalTerminal) next.terminal = record.cursor;
    return next;
  });
  const faults = { ...view.faults };
  if (faults.reconciliation && record.cursor > faults.reconciliation.after && event.type === 'reconciled') {
    const fault = faults.reconciliation;
    const callId = event.reconciliation?.call_id;
    if (typeof callId === 'string' && fault.callIds.includes(callId)) {
      const callIds = fault.callIds.filter(id => id !== callId);
      faults.reconciliation = { ...fault, callIds, terminal: callIds.length ? null : record.cursor };
    }
  }
  let activeAttempt = view.activeAttempt;
  if (event.type === 'attempt_started' && event.lease?.session_id === view.id && (!activeAttempt || record.cursor > activeAttempt.after)) {
    activeAttempt = { lease:event.lease, after:record.cursor, terminal:null };
  } else if (activeAttempt && ['attempt_finished', 'attempt_recovered'].includes(event.type) && record.cursor > activeAttempt.after && sameLease(activeAttempt.lease,event.lease ?? record.attempt)) {
    activeAttempt = { ...activeAttempt, terminal:record.cursor };
  }
  return { ...view, writes, faults, activeAttempt };
}

function writeNotice(view) {
  if (view?.writes.some(write => write.phase === 'uncertain')) return `连接中断，请求回执待确认。此会话（${clip(view.id, 160)}）的发送和 Fork 已暂停。当前 Runtime 不提供可查询的请求编号，后续执行无法证明这次请求的结果。请查看下方事件记录，并由宿主核对原请求是否执行及真实副作用；请勿重发、重复取消或通过刷新绕过。可在左侧新建会话处理独立任务，原会话保留等待核对。`;
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

export function applySnapshot(view, snapshot, initial = false, requestedAfter = view.cursor, requestedEpoch = view.proofEpoch) {
  if (!object(snapshot) || snapshot.id !== view.id || !Array.isArray(snapshot.messages) || !safeCursor(snapshot.cursor)) {
    throw new Error('Runtime 返回的会话快照不完整；保留当前会话和事件游标。');
  }
  // Events and snapshots share one durable cursor. A delayed projection must
  // never recreate pending calls/attempts after their terminal facts were read.
  if (view.snapshot && snapshot.cursor < Math.max(view.snapshot.cursor, view.cursor)) return view;
  const faults = { ...view.faults };
  if (frozen(snapshot)) faults.frozen = { after: snapshot.cursor };
  if (unresolved(snapshot)) {
    const callIds = array(snapshot.unresolved_calls).map(call => call.call?.id).filter(id => typeof id === 'string');
    if (!faults.reconciliation) faults.reconciliation = { after: snapshot.cursor, callIds, terminal: null };
    else if (callIds.length) {
      const fault = faults.reconciliation;
      const added = callIds.filter(id => !fault.callIds.includes(id));
      // Re-reading the same pending calls preserves already consumed evidence
      // for the remaining calls. Only genuinely new ids add an obligation.
      faults.reconciliation = { ...fault, callIds: [...fault.callIds, ...added], terminal: added.length ? null : fault.terminal };
    }
  }
  // A Core snapshot may predate the runtime binding view. Its missing metadata
  // never clears an explicit frozen rejection, even at the same durable cursor.
  return settleView({
    ...view,
    faults,
    activeAttempt: object(snapshot.attempt) && !sameLease(snapshot.attempt,view.activeAttempt?.lease) ? { lease:snapshot.attempt, after:snapshot.cursor, terminal:null } : view.activeAttempt,
    snapshot: { ...snapshot, messages: snapshot.messages.slice(-MAX_MESSAGES) },
    totalMessages: snapshot.messages.length,
    snapshotAfter: Math.max(view.snapshotAfter, requestedAfter),
    snapshotEpoch: Math.max(view.snapshotEpoch, requestedEpoch),
    cursor: initial ? snapshot.cursor : view.cursor,
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
  let next = { ...view, events: [...view.events] };
  for (const record of records) {
    if (record.cursor <= view.cursor) continue;
    const event = declaredAgentEvent(record.event ?? record);
    if (event.type === 'text_delta' && typeof event.delta === 'string') {
      next.liveText = `${next.liveText}${event.delta}`.slice(-MAX_TEXT);
    } else if (event.type === 'message_update' && object(event.message)) {
      // AgentEvent carries the actual partial assistant message. Never animate
      // snapshot text or guess that an unfamiliar event is streaming output.
      next.liveText = textOnly(event.message).slice(-MAX_TEXT);
    } else if (event.type === 'message_start' && event.message?.role === 'assistant') {
      next.liveText = '';
    } else if ((event.type === 'message_end' && event.message?.role === 'assistant') || event.type === 'agent_end') {
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
function unresolved(snapshot) {
  const calls = array(snapshot?.unresolved_calls);
  const unknownCall = calls.some(call => !object(call) || !['planned', 'prepared', 'started'].includes(call.state) || call.result?.outcome_unknown);
  return Boolean(snapshot?.needs_reconciliation) || unknownCall || (calls.length > 0 && !sessionRunning(snapshot)) || stateName(snapshot).includes('reconcil') || stateName(snapshot).includes('unknown');
}
function frozen(snapshot, view) {
  return Boolean(view?.faults.frozen || snapshot?.binding?.frozen) || /frozen|unavailable/.test(stateName(snapshot));
}
export function sessionRunning(snapshot) {
  return Boolean(snapshot?.is_running || snapshot?.attempt) || /running|awaiting_approval|waiting_approval/.test(stateName(snapshot));
}
export function sessionBlocked(snapshot, view) {
  if (!snapshot) return '请先选择会话。';
  if (unresolved(snapshot) || view?.faults.reconciliation) return '存在尚未核对的工具副作用。请由宿主核对真实结果；网页无法清除该状态，也不会自动重试。';
  if (writeNotice(view)) return writeNotice(view);
  if (frozen(snapshot, view)) return '强流程目前不可用，此会话保持冻结。可明确确认后 Fork 为新的普通聊天会话。';
  if (!['idle', 'running', 'awaiting_approval'].includes(stateName(snapshot))) return '会话状态尚未确认，请等待 Runtime 同步后再发送。';
  return '';
}
function canFork(view) {
  return Boolean(view?.snapshot && frozen(view.snapshot, view) && !unresolved(view.snapshot) && !view.faults.reconciliation && !view.writes.length && !view.activeAttempt && !sessionRunning(view.snapshot) && !array(view.snapshot.pending_approvals).length);
}
function stateLabel(snapshot, view) {
  if (unresolved(snapshot) || view?.faults.reconciliation) return '待核对副作用';
  if (view?.writes.some(write => write.phase === 'uncertain')) return '连接结果待确认';
  if (array(snapshot?.pending_approvals).length) return '等待审批';
  if (view?.writes.length) return '正在处理';
  if (frozen(snapshot, view)) return '强流程冻结';
  const name = stateName(snapshot);
  return ({ idle: '就绪', ready: '就绪', running: '运行中', cancelled: '已取消', failed: '执行失败', completed: '已完成', unknown: '状态待确认' })[name] ?? clip(name, 60);
}

if (typeof document !== 'undefined') boot();

function boot() {
  const $ = id => document.getElementById(id);
  const ui = Object.fromEntries([
    'login-panel', 'login-form', 'login-error', 'login-button', 'username', 'password', 'account-fields', 'workspace', 'logout', 'connection-status',
    'create-form', 'session-label', 'session-device', 'create-button', 'refresh-sessions', 'list-status', 'session-list', 'session-picker', 'session-picker-label',
    'conversation', 'session-title', 'session-state', 'global-error', 'poll-status', 'messages', 'history-note',
    'blocked-notice', 'blocked-text', 'open-fork', 'approvals', 'live-section', 'live-text', 'event-log', 'event-count',
    'prompt-form', 'prompt', 'prompt-status', 'send-button', 'cancel-button', 'fork-dialog', 'fork-form',
    'fork-label', 'fork-confirm', 'fork-error', 'fork-button', 'close-fork',
    'model-budget-dialog', 'model-budget-form', 'model-budget-summary', 'model-budget-confirm', 'model-budget-error', 'continue-model-budget', 'close-model-budget',
    'refresh-capabilities', 'capability-content',
    'panel-sidebar', 'panel-sidebar-status', 'panel-sidebar-guide', 'panel-sidebar-open', 'refresh-panels',
    'panel-bottom', 'panel-bottom-status', 'panel-bottom-guide', 'panel-bottom-open',
    'wb-tasks', 'wb-usage', 'wb-artifacts', 'wb-plannotator', 'wb-editor', 'wb-trace',
  ].map(id => [id, $(id)]));
  let authMode = 'local';
  let loginRequired = false;
  let connected = false;
  let workspaceId = null;
  let workspaceBusy = false;
  const sessionWorkspaces = new Map();
  let toolAvailability = {};
  let loginGeneration = 0;
  let selected = null;
  let sessions = [];
  let views = new Map();
  let promptPending = new Set();
  let approvalPending = new Set();
  const taskAdmissions = new Map();
  let budgetDecision = null;
  let selectionGeneration = 0;
  let listGeneration = 0;
  let promptRevision = 0;
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
    session_frozen: '强流程当前不可用，会话保持冻结。请在会话提示中确认后创建普通聊天分支。',
    needs_reconciliation: '工具副作用尚未核对，需要由宿主检查真实结果。',
    needs_reconciliation_outcome_unknown: '工具副作用尚未核对，需要由宿主检查真实结果。',
    tool_outcome_unknown: '工具结果尚未核对，需要由宿主检查真实副作用。',
  })[error?.code] ?? (error?.outcome_unknown ? '连接中断或请求结果待确认。页面不会自动重发，请等待事件同步或由宿主核对。' : error?.message ?? '发生未知错误。'), 2500);
  const report = error => {
    ui['global-error'].textContent = error ? errorText(error) : '';
    ui['global-error'].hidden = !error;
  };
  const activeView = () => selected ? views.get(selected) : undefined;
  const setPromptValue = value => {
    const next = String(value);
    if (ui['prompt'].value !== next) {
      ui['prompt'].value = next;
      promptRevision++;
    }
  };

  function finishBudgetDecision(approved) {
    const pending = budgetDecision;
    budgetDecision = null;
    if (ui['model-budget-dialog'].open) ui['model-budget-dialog'].close();
    ui['model-budget-confirm'].checked = false;
    ui['model-budget-error'].textContent = '';
    if (pending) pending.resolve(approved ? pending.proposal : null);
  }

  function confirmModelBudget(status) {
    const proposal = status?.continuation;
    if (!status?.budget_unknown || !object(proposal) || !Array.isArray(proposal.expected)
      || !Number.isSafeInteger(proposal.additional_tokens) || proposal.additional_tokens <= 0
      || !Number.isSafeInteger(proposal.token_limit) || proposal.token_limit <= 0) {
      throw new RpcError('invalid_response', 'Runtime 返回的模型预算恢复方案无效；没有创建新任务。');
    }
    finishBudgetDecision(false);
    ui['model-budget-summary'].textContent = `先前有 ${proposal.unknown_request_count} 个模型请求用量未知，保留预留 ${status.reserved_tokens} token。本次只为一个新任务增加最多 ${proposal.additional_tokens} token；累计上限变为 ${proposal.token_limit} token。`;
    ui['model-budget-confirm'].checked = false;
    ui['model-budget-error'].textContent = '';
    ui['model-budget-dialog'].showModal();
    return new Promise(resolve => { budgetDecision = { proposal, resolve }; });
  }

  ui['model-budget-form'].addEventListener('submit', event => {
    event.preventDefault();
    if (!budgetDecision) return;
    if (!ui['model-budget-confirm'].checked) {
      ui['model-budget-error'].textContent = '请先确认保留未知预留并增加列明的有限额度。';
      return;
    }
    finishBudgetDecision(true);
  });
  ui['close-model-budget'].addEventListener('click', () => finishBudgetDecision(false));
  ui['model-budget-dialog'].addEventListener('cancel', event => {
    event.preventDefault();
    finishBudgetDecision(false);
  });

  async function rpc(method, params = {}, scope = sessionWorkspaces.get(params.session_id) || workspaceId) {
    const controller = new AbortController();
    controllers.add(controller);
    let timeout;
    if (!['session.prompt', 'task.resume'].includes(method)) timeout = setTimeout(() => controller.abort(), method === 'settings.workspaces.open' ? 45000 : 15000);
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
      clearTimeout(timeout);
      controllers.delete(controller);
    }
  }


  let panelCatalog = null;
  let openedPanels = [];
  let openedPanelsSession = null;
  const panelSessionId = () => selected || 'webui-local';

  async function panelsApi(method, params = {}) {
    const controller = new AbortController();
    controllers.add(controller);
    const timeout = setTimeout(() => controller.abort(), 15000);
    try {
      const response = await fetch('/api/panels', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ method, params }),
        signal: controller.signal,
        cache: 'no-store',
        credentials: 'same-origin',
      });
      let payload;
      try { payload = await response.json(); } catch { throw new RpcError('invalid_response', '面板接口返回了无效 JSON。', false, response.status); }
      if (!response.ok || payload?.error) {
        const detail = payload?.error;
        throw new RpcError(typeof detail?.code === 'string' ? detail.code : 'http_error', detail?.code === 'forbidden' && response.status === 403 ? '登录已失效，请重新输入账号密码。' : detail?.message ?? `面板请求失败（HTTP ${response.status}）。`, false, response.status);
      }
      if (!object(payload) || !Object.hasOwn(payload, 'result')) throw new RpcError('invalid_response', '面板响应缺少 result。', false, response.status);
      return payload.result;
    } catch (error) {
      if (error.name === 'AbortError') throw new RpcError('transport_unknown', '面板请求连接已中断或超时。', false);
      if (error instanceof TypeError) throw new RpcError('transport_unknown', '无法连接本机面板接口。', false);
      throw error;
    } finally {
      clearTimeout(timeout);
      controllers.delete(controller);
    }
  }

  function renderPanelGuide(target, entries, zone) {
    target.replaceChildren();
    for (const panel of array(entries)) {
      const button = el('button', 'secondary', panel.title || panel.id);
      button.type = 'button';
      button.dataset.panelId = panel.id;
      button.title = panel.description || panel.id;
      button.addEventListener('click', () => openRegisteredPanel(panel.id, zone));
      target.append(button);
    }
  }

  function renderOpenedPanels() {
    const sidebarOpen = ui['panel-sidebar-open'];
    const bottomOpen = ui['panel-bottom-open'];
    sidebarOpen.replaceChildren();
    bottomOpen.replaceChildren();
    const currentSession = panelSessionId();
    const activePanels = openedPanelsSession === currentSession ? openedPanels.filter(p => !p.session_id || p.session_id === currentSession) : [];
    for (const opened of activePanels) {
      const card = el('div', 'panel-card');
      card.dataset.active = opened.active ? 'true' : 'false';
      card.append(el('strong', '', opened.title || opened.panel_id));
      card.append(el('span', 'small muted', `${opened.panel_id}${opened.path ? ` · ${opened.path}` : ''}`));
      const actions = el('div', 'actions');
      const activate = el('button', 'quiet', '激活');
      activate.type = 'button';
      activate.addEventListener('click', () => activateOpenedPanel(opened.instance_id));
      const close = el('button', 'quiet', '关闭');
      close.type = 'button';
      close.addEventListener('click', () => closeOpenedPanel(opened.instance_id));
      actions.append(activate, close);
      if (opened.panel_id === 'files') {
        const reveal = el('button', 'quiet', '揭示路径');
        reveal.type = 'button';
        reveal.addEventListener('click', () => interactOpenedPanel(opened.instance_id, 'editor', opened.path || 'README.md'));
        actions.append(reveal);
      }
      if (opened.panel_id === 'terminal-bottom') {
        const focus = el('button', 'quiet', '聚焦终端');
        focus.type = 'button';
        focus.addEventListener('click', () => interactOpenedPanel(opened.instance_id, 'terminal'));
        actions.append(focus);
      }
      card.append(actions);
      if (opened.active) {
        renderActivePanelContent(card, opened);
      }
      if (opened.zone === 'bottom') bottomOpen.append(card);
      else sidebarOpen.append(card);
    }
    ui['panel-bottom'].hidden = !(array(panelCatalog?.bottom).length || activePanels.some(item => item.zone === 'bottom'));
    ui['panel-bottom-status'].textContent = activePanels.filter(item => item.zone === 'bottom').length ? `${activePanels.filter(item => item.zone === 'bottom').length} 个已打开` : '可从目录打开底部面板';
  }

  function renderActivePanelContent(container, opened) {
    if (!opened.active) return;
    const body = el('div', 'panel-body');
    const panelId = opened.panel_id;

    if (panelId === 'editor') {
      const cfgCard = el('div', 'item-card');
      cfgCard.append(el('strong', '', '可视化编辑器服务配置'));

      const addrId = `ed-addr-${opened.instance_id}`;
      const addrLabel = el('label', 'small', '本地服务地址');
      addrLabel.htmlFor = addrId;
      const addrInput = el('input');
      addrInput.id = addrId;
      addrInput.type = 'text';
      addrInput.placeholder = '粘贴编辑器启动时输出的 editor_url';
      addrInput.value = opened.editor_addr || '';

      const pathId = `ed-path-${opened.instance_id}`;
      const pathLabel = el('label', 'small', '宿主记录的文件路径');
      pathLabel.htmlFor = pathId;
      const pathInput = el('input');
      pathInput.id = pathId;
      pathInput.type = 'text';
      pathInput.placeholder = 'index.html';
      pathInput.value = opened.path || 'index.html';

      const actionsRow = el('div', 'control-row');
      const openBtn = el('button', 'primary', '新窗口打开编辑器服务');
      openBtn.type = 'button';
      openBtn.disabled = true;
      const validateAddress = () => { try { localServiceUrl(addrInput.value.trim()); openBtn.disabled = false; } catch { openBtn.disabled = true; } };
      addrInput.addEventListener('input', validateAddress);
      validateAddress();
      openBtn.addEventListener('click', () => {
        try {
          const url = buildVisualEditorLaunchUrl({ baseUrl: addrInput.value.trim() });
          window.open(url, '_blank', 'noopener,noreferrer');
        } catch (error) { handleError(error); }
      });

      const updatePathBtn = el('button', 'secondary', '更新宿主记录');
      updatePathBtn.type = 'button';
      updatePathBtn.addEventListener('click', () => interactOpenedPanel(opened.instance_id, 'editor', pathInput.value.trim()));

      actionsRow.append(openBtn, updatePathBtn);
      const notice = el('p', 'small muted', `${toolAvailability.editor?.installed ? '已安装，需单独启动服务。' : '未检测到同目录安装；请先安装或提供已启动的本机服务地址。'} cargo install --path crates/cayw-visual-editor --bin cayw-visual-editor --locked；cayw-visual-editor --help。 在新窗口中输入 CAYW_EDITOR_TOKEN 登录。编辑项目和页面由编辑器启动参数确定；这里的文件路径只更新宿主记录。主网页令牌和 IPC 凭据不会传给编辑器。`);
      cfgCard.append(addrLabel, addrInput, pathLabel, pathInput, actionsRow, notice);
      body.append(cfgCard);
    } else if (panelId === 'trace') {
      const cfgCard = el('div', 'item-card');
      cfgCard.append(el('strong', '', 'Trace Debug 外部调试服务'));

      const addrId = `trace-addr-${opened.instance_id}`;
      const addrLabel = el('label', 'small', '本地 Trace Debug 服务地址');
      addrLabel.htmlFor = addrId;
      const addrInput = el('input');
      addrInput.id = addrId;
      addrInput.type = 'text';
      addrInput.placeholder = '粘贴 Trace 启动时输出的 web_url';
      addrInput.value = opened.trace_addr || '';

      const actionsRow = el('div', 'control-row');
      const openBtn = el('button', 'primary', '新窗口打开 Trace Debug');
      openBtn.type = 'button';
      openBtn.disabled = true;
      const validateAddress = () => { try { localServiceUrl(addrInput.value.trim()); openBtn.disabled = false; } catch { openBtn.disabled = true; } };
      addrInput.addEventListener('input', validateAddress);
      validateAddress();
      openBtn.addEventListener('click', () => {
        try {
          const url = buildTraceDebugLaunchUrl({ baseUrl: addrInput.value.trim() });
          window.open(url, '_blank', 'noopener,noreferrer');
        } catch (error) { handleError(error); }
      });
      actionsRow.append(openBtn);
      const notice = el('p', 'small muted', `${toolAvailability.trace?.installed ? '已安装，需单独启动服务。' : '未检测到同目录安装；请先安装或提供已启动的本机服务地址。'} cargo install --path crates/cayw-trace-debug --bin cayw-trace-debug --locked；cayw-trace-debug --help。 在新窗口中输入 CAYW_TRACE_WEB_TOKEN 登录。采集的会话由 Trace 启动参数确定。主网页令牌和 IPC 凭据不会传给 Trace 页面。`);
      cfgCard.append(addrLabel, addrInput, actionsRow, notice);

      const inspectCard = el('div', 'item-card');
      const inspectH = el('div', 'control-row');
      inspectH.append(el('strong', '', '会话执行透视与重放追踪 (session.run_inspector)'));
      const refreshBtn = el('button', 'secondary', '刷新执行步骤');
      refreshBtn.type = 'button';
      inspectH.append(refreshBtn);

      const inspectContent = el('div', 'item-list');
      inspectContent.textContent = '点击刷新读取会话执行步骤与恢复时间线…';

      const loadInspect = async () => {
        const targetSessionId = panelSessionId();
        const generation = selectionGeneration;
        const login = loginGeneration;
        try {
          inspectContent.textContent = '正在读取执行透视…';
          const res = await rpc('session.run_inspector', { session_id: targetSessionId, after: 0, limit: 100 });
          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }
          inspectContent.replaceChildren();

          const metaP = el('p', 'small muted', `扫描事件: ${res?.events_scanned ?? 0} / 总事件: ${res?.events_total ?? 0}${res?.has_more ? ' (存在更多)' : ''} · 协议版本: ${res?.protocol_version ?? 2}`);
          inspectContent.append(metaP);

          const steps = array(res?.snapshot?.steps);
          if (!steps.length) {
            inspectContent.append(el('p', 'small muted', '当前会话暂无执行步骤记录。'));
          } else {
            const stepsTable = el('div', 'item-list');
            for (const step of steps) {
              const sc = el('div', 'item-card');
              const sh = el('div', 'control-row');
              sh.append(el('strong', '', `[轮次 ${step.turn} · 序 ${step.ordinal}] ${step.tool || 'model'}`));
              const badge = el('span', `badge ${step.outcome || ''}`, step.outcome || 'unknown');
              sh.append(badge);
              sh.append(el('span', 'small muted', `${step.elapsed_ms || 0}ms`));
              sc.append(sh);
              if (step.error_summary) {
                sc.append(el('p', 'small error', `错误: ${step.error_summary}`));
              }
              if (step.params_digest) {
                sc.append(el('p', 'small muted', `参数指纹: ${clip(step.params_digest, 40)}`));
              }
              stepsTable.append(sc);
            }
            inspectContent.append(stepsTable);
          }

          const paths = array(res?.timeline?.paths);
          if (paths.length) {
            const pathCard = el('div', 'item-card');
            pathCard.append(el('strong', '', `恢复重试路径 (${paths.length} 条)`));
            for (const p of paths) {
              pathCard.append(el('p', 'small', `[${p.kind}] 步骤 ${p.failed_step_id} ➔ ${p.recovered_step_id || '未恢复'}`));
            }
            inspectContent.append(pathCard);
          }
        } catch (err) {
          inspectContent.textContent = '读取执行透视失败：' + (err.message || String(err));
        }
      };

      refreshBtn.addEventListener('click', loadInspect);
      inspectCard.append(inspectH, inspectContent);
      body.append(cfgCard, inspectCard);
    } else if (panelId === 'artifacts') {
      const topActions = el('div', 'control-row');
      const refreshBtn = el('button', 'secondary', '刷新产物');
      refreshBtn.type = 'button';
      const saveRecoveryBtn = el('button', 'quiet', '保存快照');
      saveRecoveryBtn.type = 'button';
      const loadRecoveryBtn = el('button', 'quiet', '恢复快照');
      loadRecoveryBtn.type = 'button';
      topActions.append(refreshBtn, saveRecoveryBtn, loadRecoveryBtn);

      const statusP = el('p', 'small muted', '');
      const artContainer = el('div', 'item-list');
      artContainer.textContent = '正在读取会话产物…';

      saveRecoveryBtn.addEventListener('click', async () => {
        try {
          statusP.textContent = '正在保存产物快照…';
          const res = await rpc('artifacts.recovery_save', {});
          statusP.textContent = `产物快照已保存到磁盘恢复点 (版本 ${res?.version ?? 1})。`;
        } catch (err) {
          statusP.textContent = '保存快照失败：' + (err.message || String(err));
        }
      });

      loadRecoveryBtn.addEventListener('click', async () => {
        try {
          statusP.textContent = '正在从磁盘恢复产物快照…';
          await rpc('artifacts.recovery_load', {});
          statusP.textContent = '产物快照恢复完成。';
          await loadArts();
        } catch (err) {
          statusP.textContent = '恢复快照失败：' + (err.message || String(err));
        }
      });

      const loadArts = async () => {
        const targetSessionId = panelSessionId();
        const generation = selectionGeneration;
        const login = loginGeneration;
        try {
          const list = await rpc('artifacts.list', { session_id: targetSessionId });
          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }
          artContainer.replaceChildren();
          if (!array(list).length) {
            artContainer.textContent = '当前会话暂无生成产物。';
            return;
          }
          for (const item of list) {
            const itemCard = el('div', 'item-card');
            const header = el('div', 'control-row');
            header.append(el('strong', '', item.title || item.artifact_id));
            header.append(el('span', 'badge', item.kind || 'card'));
            header.append(el('span', 'small muted', `v${item.content_version || 1}`));

            const previewBox = el('div', 'artifact-preview-box');
            previewBox.textContent = clip(item.body || '', 500);

            const previewRow = el('div', 'control-row');
            const previewBtn = el('button', 'quiet', '获取渲染投影');
            previewBtn.type = 'button';
            const previewResult = el('div', 'small muted');
            previewBtn.addEventListener('click', async () => {
              try {
                previewResult.textContent = '正在生成投影…';
                const p = await rpc('artifacts.preview', { artifact_id: item.artifact_id });
                previewResult.textContent = `类型: ${p?.content_type || 'text/html'}, 字符数: ${(p?.html || '').length}`;
              } catch (err) {
                previewResult.textContent = '生成投影失败：' + (err.message || String(err));
              }
            });
            previewRow.append(previewBtn, previewResult);

            const actionRow = el('div', 'control-row');
            const promptId = `art-prompt-${item.artifact_id}`;
            const promptLabel = el('label', 'small', '后续指令');
            promptLabel.htmlFor = promptId;
            const promptInput = el('input');
            promptInput.id = promptId;
            promptInput.type = 'text';
            promptInput.placeholder = '输入后续任务动作指令';
            promptInput.value = `根据产物“${item.title}”继续执行后续任务`;

            const actionBtn = el('button', 'secondary', '转发动作到 Runtime');
            actionBtn.type = 'button';
            actionBtn.addEventListener('click', async () => {
              statusP.textContent = '正在转发动作到 Runtime…';
              try {
                const actionRes = await rpc('artifacts.action', {
                  session_id: panelSessionId(),
                  artifact_id: item.artifact_id,
                  action_id: `action-${Date.now()}`,
                  kind: 'button',
                  intent: { type: 'follow_up_task', prompt: promptInput.value.trim() },
                  expected_content_version: item.content_version,
                  requested_at_ms: Date.now(),
                });
                statusP.textContent = `动作已转发！状态：${actionRes?.record?.status || 'applied'}`;
              } catch (err) {
                statusP.textContent = '动作转发失败：' + (err.message || String(err));
              }
            });
            actionRow.append(promptLabel, promptInput, actionBtn);
            itemCard.append(header, previewBox, previewRow, actionRow);
            artContainer.append(itemCard);
          }
        } catch (err) {
          artContainer.textContent = '读取产物失败：' + (err.message || String(err));
        }
      };

      refreshBtn.addEventListener('click', loadArts);
      loadArts();
      body.append(topActions, statusP, artContainer);
    } else if (panelId === 'plannotator') {
      const planInfoCard = el('div', 'item-card');
      planInfoCard.append(el('strong', '', '权威共享计划状态 (session.plan)'));
      const reviewedSessionId = panelSessionId();
      const planFactP = el('p', 'small muted', '正在读取共享版本事实…');
      const planSteps = el('div', 'item-list');
      const reloadPlanBtn = el('button', 'secondary', '重新读取计划并清空意见');
      reloadPlanBtn.type = 'button';
      planInfoCard.append(planFactP, planSteps, reloadPlanBtn);

      const form = el('form', 'item-card');
      form.append(el('strong', '', '提交版本绑定批注'));

      const decisionId = `plann-dec-${opened.instance_id}`;
      const decisionLabel = el('label', 'small', '决策类型');
      decisionLabel.htmlFor = decisionId;
      const decisionSelect = el('select');
      decisionSelect.id = decisionId;
      for (const [val, label] of [
        ['approve', '批准继续 (approve)'],
        ['deny_with_feedback', '拒绝并反馈 (deny_with_feedback)'],
        ['approve_with_notes', '带意见批准 (approve_with_notes)']
      ]) {
        const opt = el('option', '', label);
        opt.value = val;
        decisionSelect.append(opt);
      }
      decisionSelect.value = 'approve';

      const summaryId = `plann-sum-${opened.instance_id}`;
      const summaryLabel = el('label', 'small', '批注摘要');
      summaryLabel.htmlFor = summaryId;
      const summaryInput = el('input');
      summaryInput.id = summaryId;
      summaryInput.placeholder = '批注摘要 (如 架构设计确认)';
      summaryInput.value = '计划审查确认';

      const feedbackId = `plann-fb-${opened.instance_id}`;
      const feedbackLabel = el('label', 'small', '审查意见');
      feedbackLabel.htmlFor = feedbackId;
      const feedbackInput = el('textarea');
      feedbackInput.id = feedbackId;
      feedbackInput.placeholder = '输入针对该计划版本的审查反馈意见…';
      feedbackInput.rows = 2;
      feedbackInput.value = '审查通过，允许继续执行';

      const submitBtn = el('button', '', '提交版本绑定批注');
      submitBtn.type = 'submit';
      submitBtn.disabled = true;
      const submitStatus = el('p', 'small muted', '');

      form.append(
        decisionLabel, decisionSelect,
        summaryLabel, summaryInput,
        feedbackLabel, feedbackInput,
        submitBtn, submitStatus
      );

      let currentPlan = null;
      const loadPlanAndFeedback = async () => {
        const targetSessionId = panelSessionId();
        const generation = selectionGeneration;
        const login = loginGeneration;
        try {
          const plan = await rpc('session.plan', { session_id: targetSessionId });
          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }
          currentPlan = plan;
          planSteps.replaceChildren();
          if (currentPlan && currentPlan.task_id) {
            validatePlanFeedback(currentPlan, { session_id: targetSessionId, decision: 'approve' });
            planFactP.textContent = `关联任务: ${currentPlan.task_id} · 共享版本: v${currentPlan.version} · 内容摘要: ${currentPlan.content_sha256}`;
            for (const step of array(currentPlan.steps)) {
              planSteps.append(el('p', 'small', `[${step.status}] ${step.text}`));
            }
            submitBtn.disabled = false;
          } else {
            planFactP.textContent = '当前会话暂无排队或执行中的计划（需在会话中生成计划后方可绑定版本批注）。';
            submitBtn.disabled = true;
          }
        } catch (err) {
          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }
          planFactP.textContent = '读取计划状态失败：' + (err.message || String(err));
          submitBtn.disabled = true;
        }
        await loadFeedback();
      };

      reloadPlanBtn.addEventListener('click', async () => {
        currentPlan = null;
        submitBtn.disabled = true;
        decisionSelect.value = 'approve';
        summaryInput.value = '';
        feedbackInput.value = '';
        submitStatus.textContent = '意见已清空，请审阅重新读取的计划。';
        await loadPlanAndFeedback();
      });

      form.addEventListener('submit', async event => {
        event.preventDefault();
        if (submitBtn.disabled || !currentPlan) return;
        submitBtn.disabled = true;
        submitStatus.textContent = '正在提交批注…';
        try {
          const payload = validatePlanFeedback(currentPlan, {
            session_id: panelSessionId(),
            decision: decisionSelect.value,
            summary: summaryInput.value.trim() || '计划审查批注',
            comments: [{
              comment_id: `c-${Date.now()}`,
              body: feedbackInput.value.trim() || '审查意见',
              created_at_ms: Date.now(),
            }],
          });
          payload.actor = 'webui-user';
          const res = await rpc('plannotator.submit', payload);
          submitStatus.textContent = `批注已提交！ID: ${res?.feedback_id || 'ok'}，已绑定版本 v${res?.target?.file?.version || currentPlan.version}。后续执行会再次核对计划版本。`;
          await loadFeedback();
        } catch (err) {
          submitStatus.textContent = '提交批注失败：' + (err.message || String(err)) + '。意见已保留；重新审阅前不会自动改绑版本。';
        }
      });

      const listContainer = el('div', 'item-list');
      const loadFeedback = async () => {
        const targetSessionId = panelSessionId();
        const generation = selectionGeneration;
        const login = loginGeneration;
        try {
          const items = await rpc('plannotator.list', { session_id: targetSessionId });
          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }
          listContainer.replaceChildren();
          if (!array(items).length) {
            listContainer.textContent = '当前会话暂无批注记录。';
            return;
          }
          for (const fb of items) {
            const card = el('div', 'item-card');
            const h = el('div', 'control-row');
            h.append(el('strong', '', fb.target?.target_id || fb.target || 'target'));
            h.append(el('span', `badge ${fb.decision || ''}`, fb.decision || 'apply'));
            const verInfo = fb.target?.file ? `${fb.target.file.path} (v${fb.target.file.version})` : '';
            card.append(h, el('p', 'small', fb.summary || ''), el('span', 'small muted', verInfo));
            listContainer.append(card);
          }
        } catch (err) {
          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }
          listContainer.textContent = '读取批注失败：' + (err.message || String(err));
        }
      };

      loadPlanAndFeedback();
      body.append(planInfoCard, form, el('label', 'small', '已保存批注记录'), listContainer);
    } else if (panelId === 'tasks') {
      const refreshBtn = el('button', 'secondary', '刷新任务与子 Agent Fleet');
      refreshBtn.type = 'button';
      const statusP = el('p', 'small muted', '');
      const contentBox = el('div', 'item-list');
      contentBox.textContent = '正在读取任务队列与子 Agent…';

      const loadTasksAndFleet = async () => {
        const targetSessionId = panelSessionId();
        const generation = selectionGeneration;
        const login = loginGeneration;
        contentBox.textContent = '正在读取数据…';
        try {
          let tasks = null;
          let tasksError = null;
          try {
            tasks = await rpc('session.tasks', { session_id: targetSessionId });
          } catch (err) {
            if (err.auth) { handleError(err); return; }
            tasksError = err;
          }

          let plan = null;
          let planError = null;
          try {
            plan = await rpc('session.plan', { session_id: targetSessionId });
          } catch (err) {
            if (err.auth) { handleError(err); return; }
            planError = err;
          }

          let fleet = null;
          let fleetError = null;
          try {
            fleet = await rpc('runtime.fleet', { parent_session_id: targetSessionId });
          } catch (err) {
            if (err.auth) { handleError(err); return; }
            fleetError = err;
          }

          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }

          contentBox.replaceChildren();

          if (planError) {
            const planBox = el('div', 'item-card');
            planBox.append(el('strong', '', '当前计划 (读取失败)'));
            planBox.append(el('p', 'small error', `读取计划失败：${errorText(planError)}`));
            contentBox.append(planBox);
          } else if (plan?.steps && Array.isArray(plan.steps)) {
            const planBox = el('div', 'item-card');
            planBox.append(el('strong', '', `当前计划 (v${plan.version || 1}) · 任务: ${plan.task_id || 'unknown'}`));
            for (const step of plan.steps) {
              const stepP = el('p', 'small', `[${step.status || 'pending'}] ${step.text || step.id}`);
              planBox.append(stepP);
            }
            contentBox.append(planBox);
          }

          if (tasksError) {
            const tasksCard = el('div', 'item-card');
            tasksCard.append(el('strong', '', '会话任务队列 (读取失败)'));
            tasksCard.append(el('p', 'small error', `读取任务队列失败：${errorText(tasksError)}`));
            contentBox.append(tasksCard);
          } else {
            const taskItems = array(tasks);
            const tasksCard = el('div', 'item-card');
            tasksCard.append(el('strong', '', `会话任务队列 (${taskItems.length})`));
            if (!taskItems.length) {
              tasksCard.append(el('p', 'small muted', '当前会话暂无排队任务。'));
            } else {
              for (const t of taskItems) {
                const tRow = el('div', 'control-row');
                tRow.append(el('strong', '', t.id || 'task'));
                tRow.append(el('span', 'badge', t.state || 'queued'));
                tRow.append(el('span', 'small muted', clip(t.input?.text || '', 60)));
                tasksCard.append(tRow);
              }
            }
            contentBox.append(tasksCard);
          }

          if (fleetError) {
            const fleetCard = el('div', 'item-card');
            fleetCard.append(el('strong', '', '子 Agent 舰队管理 (FleetView · 读取失败)'));
            fleetCard.append(el('p', 'small error', `读取子 Agent 舰队失败：${errorText(fleetError)}`));
            contentBox.append(fleetCard);
          } else {
            const members = array(fleet?.members);
            const fleetCard = el('div', 'item-card');
            fleetCard.append(el('strong', '', `子 Agent 舰队管理 (FleetView · ${members.length} 个实例)`));
            if (!members.length) {
              fleetCard.append(el('p', 'small muted', '当前会话暂无派生的子 Agent。'));
            } else {
              for (const m of members) {
                const mCard = el('div', 'item-card');
                const mh = el('div', 'control-row');
                mh.append(el('strong', '', m.label || m.child_task_id));
                mh.append(el('span', `badge ${m.state || ''}`, m.state || 'running'));
                mh.append(el('span', 'small muted', `深度: ${m.depth} · 模式: ${m.mode || 'foreground'}${m.detached ? ' (分离)' : ''}`));
                mCard.append(mh);

                if (m.error) {
                  mCard.append(el('p', 'small error', `错误: ${m.error}`));
                }

                if (m.state === 'running' || m.state === 'Running') {
                  const ctrlRow = el('div', 'control-row');
                  const stopBtn = el('button', 'secondary', '停止子 Agent');
                  stopBtn.type = 'button';
                  stopBtn.addEventListener('click', async () => {
                    statusP.textContent = `正在停止子任务 ${m.child_task_id}…`;
                    try {
                      await rpc('runtime.fleet.stop', { child_task_id: m.child_task_id });
                      statusP.textContent = `子任务 ${m.child_task_id} 已发送停止信号。`;
                      await loadTasksAndFleet();
                    } catch (err) {
                      if (err.auth) { handleError(err); return; }
                      statusP.textContent = '停止子任务失败：' + (err.message || String(err));
                    }
                  });

                  const steerInput = el('input');
                  steerInput.type = 'text';
                  steerInput.placeholder = '输入引导指令 (steer)…';
                  const steerBtn = el('button', 'quiet', '发送引导');
                  steerBtn.type = 'button';
                  steerBtn.addEventListener('click', async () => {
                    const text = steerInput.value.trim();
                    if (!text) return;
                    statusP.textContent = '正在向子任务发送引导指令…';
                    try {
                      await rpc('runtime.fleet.steer', { child_task_id: m.child_task_id, text });
                      statusP.textContent = '已向子任务发送引导指令。';
                      steerInput.value = '';
                    } catch (err) {
                      if (err.auth) { handleError(err); return; }
                      statusP.textContent = '发送引导失败：' + (err.message || String(err));
                    }
                  });

                  ctrlRow.append(stopBtn, steerInput, steerBtn);
                  mCard.append(ctrlRow);
                }

                fleetCard.append(mCard);
              }
            }
            contentBox.append(fleetCard);
          }
        } catch (err) {
          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }
          if (err.auth) { handleError(err); return; }
          contentBox.textContent = '读取任务与 Fleet 失败：' + (err.message || String(err));
        }
      };

      refreshBtn.addEventListener('click', loadTasksAndFleet);
      loadTasksAndFleet();
      body.append(refreshBtn, statusP, contentBox);
    } else if (panelId === 'usage') {
      const usageBtn = el('button', 'secondary', '刷新用量与上下文');
      usageBtn.type = 'button';
      const usageOutput = el('div', 'item-card');
      usageOutput.textContent = '正在读取会话统计…';

      const loadUsage = async () => {
        const targetSessionId = panelSessionId();
        const generation = selectionGeneration;
        const login = loginGeneration;
        try {
          const stats = await rpc('session.context_overview', { session_id: targetSessionId });
          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }
          usageOutput.replaceChildren();

          const tokens = stats.tokens || {};
          const cost = stats.cost || {};
          const cache = stats.cache || {};

          const pTokens = el('p', 'small', `Tokens 总量: ${tokens.total_tokens || 0} (输入: ${tokens.input || 0}, 输出: ${tokens.output || 0}, 推理: ${tokens.reasoning || 0})`);
          const pCache = el('p', 'small', `缓存读取: ${cache.cache_read || tokens.cache_read || 0}, 缓存写入: ${cache.cache_write || tokens.cache_write || 0}, 命中率: ${cache.hit_rate != null ? (cache.hit_rate * 100).toFixed(1) + '%' : '0.0%'}`);
          const pLedger = el('p', 'small', `账本结算: ${stats.ledger_settled_tokens || 0}, 预留: ${stats.ledger_reserved_tokens || 0}, 剩余: ${stats.ledger_remaining_tokens || 0}${stats.budget_unknown ? ' [预算未知]' : ''}`);
          const pCost = el('p', 'small', `费用结算状态: ${cost.status || 'unknown'}${cost.amount ? ` (${cost.amount} ${cost.currency || 'USD'})` : ''}`);
          const pRounds = el('p', 'small', `请求次数: ${stats.request_count || 0}, 对话轮次: ${stats.turn_count || 0}`);

          usageOutput.append(pTokens, pCache, pLedger, pCost, pRounds);

          const comp = array(stats.composition);
          if (comp.length) {
            const compCard = el('div', 'item-card');
            compCard.append(el('strong', '', 'Token 构成分析'));
            for (const c of comp) {
              compCard.append(el('p', 'small', `${c.kind}: ${c.tokens}`));
            }
            usageOutput.append(compCard);
          }

          const evo = array(stats.evolution);
          if (evo.length) {
            const evoCard = el('div', 'item-card');
            evoCard.append(el('strong', '', `上下文演变过程 (${evo.length} 轮)`));
            for (const step of evo) {
              evoCard.append(el('p', 'small muted', `[轮次 ${step.turn}] 输入: ${step.usage?.input || 0}, 输出: ${step.usage?.output || 0}, 增量: ${step.usage?.total_tokens || 0}`));
            }
            usageOutput.append(evoCard);
          }
        } catch (err) {
          if (login !== loginGeneration || generation !== selectionGeneration || targetSessionId !== panelSessionId()) {
            return;
          }
          if (err.auth) { handleError(err); return; }
          usageOutput.textContent = '读取用量失败：' + (err.message || String(err));
        }
      };

      usageBtn.addEventListener('click', loadUsage);
      loadUsage();
      body.append(usageBtn, usageOutput);
    }

    container.append(body);
  }

  async function syncPanelsForSession(sessionId, generation = selectionGeneration, login = loginGeneration) {
    if (!connected) return;
    try {
      const list = await panelsApi('panels.list_open', { session_id: sessionId });
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      openedPanels = array(list).map(p => ({ ...p, session_id: sessionId }));
      openedPanelsSession = sessionId;
      renderOpenedPanels();
    } catch (error) {
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      handleError(error);
    }
  }

  async function refreshPanels() {
    if (!connected) return;
    const login = loginGeneration;
    const generation = selectionGeneration;
    const sessionId = panelSessionId();
    ui['panel-sidebar-status'].textContent = '正在读取面板目录…';
    try {
      const catalog = await panelsApi('panels.catalog');
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      panelCatalog = catalog;
      renderPanelGuide(ui['panel-sidebar-guide'], catalog?.sidebar, 'sidebar');
      renderPanelGuide(ui['panel-bottom-guide'], catalog?.bottom, 'bottom');
      const ids = array(catalog?.sidebar).map(panel => panel.id).concat(array(catalog?.bottom).map(panel => panel.id));
      ui['panel-sidebar-status'].textContent = ids.length ? `已注册 ${[...new Set(ids)].length} 个面板` : '暂无已注册面板';
      const list = await panelsApi('panels.list_open', { session_id: sessionId });
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      openedPanels = array(list).map(p => ({ ...p, session_id: sessionId }));
      openedPanelsSession = sessionId;
      renderOpenedPanels();
    } catch (error) {
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      ui['panel-sidebar-status'].textContent = errorText(error);
      if (error.auth) disconnect(error.message);
    }
  }

  async function openRegisteredPanel(panelId, zone) {
    const login = loginGeneration;
    const generation = selectionGeneration;
    const sessionId = panelSessionId();
    report(null);
    try {
      const params = { session_id: sessionId, panel_id: panelId, zone, activate: true };
      if (panelId === 'files') params.path = 'README.md';
      await panelsApi('panels.open', params);
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      const list = await panelsApi('panels.list_open', { session_id: sessionId });
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      openedPanels = array(list).map(p => ({ ...p, session_id: sessionId }));
      openedPanelsSession = sessionId;
      renderOpenedPanels();
    } catch (error) {
      if (login === loginGeneration && generation === selectionGeneration && sessionId === panelSessionId()) handleError(error);
    }
  }

  async function activateOpenedPanel(instanceId) {
    const login = loginGeneration;
    const generation = selectionGeneration;
    const sessionId = panelSessionId();
    try {
      await panelsApi('panels.activate', { session_id: sessionId, instance_id: instanceId });
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      const list = await panelsApi('panels.list_open', { session_id: sessionId });
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      openedPanels = array(list).map(p => ({ ...p, session_id: sessionId }));
      openedPanelsSession = sessionId;
      renderOpenedPanels();
    } catch (error) {
      if (login === loginGeneration && generation === selectionGeneration && sessionId === panelSessionId()) handleError(error);
    }
  }

  async function closeOpenedPanel(instanceId) {
    const login = loginGeneration;
    const generation = selectionGeneration;
    const sessionId = panelSessionId();
    try {
      await panelsApi('panels.close', { session_id: sessionId, instance_id: instanceId });
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      const list = await panelsApi('panels.list_open', { session_id: sessionId });
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      openedPanels = array(list).map(p => ({ ...p, session_id: sessionId }));
      openedPanelsSession = sessionId;
      renderOpenedPanels();
    } catch (error) {
      if (login === loginGeneration && generation === selectionGeneration && sessionId === panelSessionId()) handleError(error);
    }
  }

  async function interactOpenedPanel(instanceId, kind, path) {
    const login = loginGeneration;
    const generation = selectionGeneration;
    const sessionId = panelSessionId();
    try {
      const params = { session_id: sessionId, instance_id: instanceId, kind };
      if (path) params.path = path;
      await panelsApi('panels.interact', params);
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      const list = await panelsApi('panels.list_open', { session_id: sessionId });
      if (login !== loginGeneration || generation !== selectionGeneration || sessionId !== panelSessionId()) return;
      openedPanels = array(list).map(p => ({ ...p, session_id: sessionId }));
      openedPanelsSession = sessionId;
      renderOpenedPanels();
    } catch (error) {
      if (login === loginGeneration && generation === selectionGeneration && sessionId === panelSessionId()) handleError(error);
    }
  }

  function disconnect(reason = '') {
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
    promptPending = new Set();
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
    ui['event-log'].replaceChildren();
    ui['approvals'].replaceChildren();
    ui['live-text'].textContent = '';
    ui['session-list'].replaceChildren();
    ui['session-picker'].open = true;
    setPromptValue('');
    ui['fork-dialog'].close();
    finishBudgetDecision(false);
    if ($('settings-dialog')?.open) $('settings-dialog').close();
    panelCatalog = null;
    openedPanels = [];
    openedPanelsSession = null;
    ui['panel-sidebar-guide'].replaceChildren();
    ui['panel-sidebar-open'].replaceChildren();
    ui['panel-bottom-guide'].replaceChildren();
    ui['panel-bottom-open'].replaceChildren();
    ui['panel-sidebar-status'].textContent = '连接后显示已注册侧栏面板。';
    ui['panel-bottom'].hidden = true;
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
      button.append(el('strong', '', clip(session.label || '未命名会话', 160)), el('span', '', `${stateLabel(session, views.get(session.id))} · ${clip(session.id, 20)}`));
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
    const { id, label, cursor, state, attempt, pending_approvals, unresolved_calls, binding } = snapshot;
    return { id, label, cursor, state, attempt, pending_approvals, unresolved_calls, binding };
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
    openedPanels = [];
    openedPanelsSession = id;
    renderOpenedPanels();
    syncPanelsForSession(id, generation, login);
    if (!views.has(id)) rememberView(createView(id));
    ui['poll-status'].textContent = '正在读取会话…';
    setPromptValue('');
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
      updateSessionSummary(snapshot);
      pollFailures = 0;
      renderView();
      if (window.matchMedia('(max-width: 640px)').matches) ui['session-picker'].open = false;
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
    ui['session-picker-label'].textContent = clip(snapshot?.label || '选择或新建', 60);
    ui['session-state'].textContent = snapshot ? stateLabel(snapshot, view) : '等待选择';
    const attention = snapshot && (unresolved(snapshot) || view?.faults.reconciliation || frozen(snapshot, view) || view?.writes.some(write => write.phase === 'uncertain') || !['idle', 'running', 'awaiting_approval'].includes(stateName(snapshot)));
    ui['blocked-text'].textContent = attention ? sessionBlocked(snapshot, view) : '';
    ui['blocked-notice'].hidden = !attention;
    ui['open-fork'].hidden = !snapshot || !frozen(snapshot, view);
    ui['open-fork'].disabled = !canFork(view);
    renderApprovals(snapshot);
    renderMessages(view);
    renderEvents(view);
    renderComposer();
  }

  function renderMessages(view) {
    const fragment = document.createDocumentFragment();
    const messages = array(view?.snapshot?.messages).filter(message => message.role !== 'system' && messageText(message).trim());
    if (!messages.length) {
      const empty = el('div', 'empty');
      empty.append(el('h2', '', view?.snapshot ? '从一条消息开始' : '继续一段对话'), el('p', '', view?.snapshot ? '输入任务，Runtime 会在这里保留对话和工具记录。' : '从左侧选择已有会话，或创建新会话。'));
      fragment.append(empty);
    }
    const expanded = new Set(view?.expandedRecords ?? []);
    const detailsFor = (key, label, text) => {
      const details = el('details', 'record-details');
      details.open = expanded.has(key);
      details.append(el('summary', '', label), el('pre', '', text));
      details.addEventListener('toggle', () => {
        if (!details.isConnected) return;
        const current = views.get(view.id);
        if (!current) return;
        const keys = new Set(current.expandedRecords ?? []);
        if (details.open) keys.add(key);
        else keys.delete(key);
        rememberView({ ...current, expandedRecords: [...keys].slice(-MAX_MESSAGES) });
      });
      return details;
    };
    for (const [index, message] of messages.entries()) {
      const role = ['user', 'assistant', 'system', 'toolResult', 'custom'].includes(message.role) ? message.role : 'custom';
      const card = el('article', `message ${role}`);
      const presentation = messagePresentation(message);
      const key = `${message.tool_call_id ?? message.timestamp ?? index}/${index}`;
      card.append(el('div', 'role', ({ user: '你', assistant: '助手', system: '系统', toolResult: '工具结果', custom: '宿主记录' })[role]));
      if (presentation.kind === 'text') card.append(el('div', 'body', presentation.text));
      else {
        card.classList.add('operation-record');
        if (presentation.kind === 'tool_call') {
          if (presentation.text) card.append(el('div', 'body', presentation.text));
          for (const [callIndex, call] of presentation.calls.entries()) {
            const title = call.name === 'execute_code' ? call.arguments?.action === 'wait' ? '等待任务结果' : '运行任务' : `使用工具：${clip(call.name, 100)}`;
            card.append(detailsFor(`${key}/${callIndex}`, title, recordText(call)));
          }
        } else {
          card.append(el('strong', `result-title ${presentation.status ?? ''}`, presentation.title));
          if (presentation.text) card.append(el('p', 'result-description', presentation.text));
          for (const value of presentation.values ?? []) {
            card.append(el('div', 'output-label', value.label), el('div', 'body output-value', value.text));
          }
          card.append(detailsFor(key, '查看完整执行记录', presentation.raw));
        }
      }
      if (message.error_message) card.append(el('p', 'effect-warning', clip(message.error_message, 2000)));
      if (message.outcome_unknown) card.append(el('p', 'effect-warning', '此工具调用的真实结果待宿主核对。'));
      fragment.append(card);
    }
    ui['messages'].replaceChildren(fragment);
    const total = view?.totalMessages ?? 0;
    ui['history-note'].textContent = total > MAX_MESSAGES ? '显示最近的对话，完整记录保存在本机。' : (messages.length ? `${messages.length} 条消息` : '');
    ui['live-text'].textContent = view?.liveText ?? '';
    ui['live-section'].hidden = !view?.liveText;
  }

  function renderEvents(view) {
    const events = array(view?.events);
    const fragment = document.createDocumentFragment();
    for (const record of events) {
      const details = el('details');
      details.append(el('summary', '', `#${record.cursor} · ${clip(record.event?.type ?? record.event?.kind ?? '事件', 100)}`), el('pre', '', recordText(record.event)));
      fragment.append(details);
    }
    ui['event-log'].replaceChildren(fragment);
    ui['event-count'].textContent = `${events.length} · 游标 ${view?.cursor ?? 0}`;
  }

  function renderComposer() {
    const view = activeView();
    const snapshot = view?.snapshot;
    const pending = promptPending.has(selected);
    const busy = pending || sessionRunning(snapshot);
    const blocked = sessionBlocked(snapshot, view);
    const decisionBusy = [...approvalPending].some(key => key.startsWith(`${selected}/`));
    ui['prompt'].disabled = !snapshot || Boolean(blocked) || busy;
    ui['send-button'].disabled = ui['prompt'].disabled || !ui['prompt'].value.trim();
    ui['send-button'].textContent = pending ? '等待完成…' : '发送';
    ui['cancel-button'].disabled = !snapshot || (!taskAdmissions.has(selected) && !view?.originalTaskId) || (!busy && !view.writes.length && !view.activeAttempt && !array(snapshot.pending_approvals).length) || ui['cancel-button'].dataset.pending === 'true';
    ui['prompt-status'].textContent = array(snapshot?.pending_approvals).length && !unresolved(snapshot) && !view?.writes.some(write => write.phase === 'uncertain') ? '请确认上方的操作请求，也可以停止运行。' : blocked || (busy ? '正在处理，可停止运行。' : decisionBusy ? '正在记录审批决定…' : 'Enter 换行，Ctrl / ⌘ + Enter 发送。');
  }

  function renderApprovals(snapshot) {
    const approvals = array(snapshot?.pending_approvals);
    const fragment = document.createDocumentFragment();
    for (const approval of approvals.slice(0, 30)) {
      const id = approval.id ?? approval.approval_id;
      const card = el('article', 'approval-card');
      const input = approval.input ?? {};
      const capability = input.capability ?? '';
      let scope;
      try { scope = JSON.parse(input.scope); } catch { scope = null; }
      const operation = ({ 'fs.read': '读取文件', 'fs.write': '写入文件', 'fs.patch': '修改文件', 'fs.list': '列出文件', 'fs.stat': '查看文件信息', 'search.query': '检索内容', 'git.status': '查看 Git 状态', 'git.diff': '查看代码改动', 'git.log': '查看提交记录', 'git.show': '查看指定提交', 'build.run': '运行预设构建', 'test.run': '运行预设测试', 'mcp.call': '调用宿主连接的工具', 'mcp.discover': '查看宿主连接的工具', 'mcp.describe': '读取工具说明', 'skills.list': '查看可用技能', 'skills.read': '读取指定技能' })[capability] ?? `使用能力：${clip(capability || '宿主未说明', 100)}`;
      card.append(el('h3', '', '请求你的授权'), el('p', '', `操作：${operation}`));
      const scopeLine = (label, value, limit = 400) => card.append(el('p', 'small', `${label}：${clip(value, limit)}`));
      if (object(scope)) {
        if (scope.path !== undefined) scopeLine('文件', scope.path);
        if (scope.profile !== undefined) scopeLine('预设任务', scope.profile);
        if (scope.query !== undefined) scopeLine('检索内容', scope.query);
        if (scope.filter !== undefined) scopeLine('测试范围', scope.filter);
        if (scope.server !== undefined) scopeLine('工具连接', scope.server);
        if (scope.tool !== undefined) scopeLine('工具', scope.tool);
        if (scope.name !== undefined) scopeLine('名称', scope.name);
        if (scope.commit !== undefined) scopeLine('提交', scope.commit);
        if (scope.overwrite === true) scopeLine('文件操作', '覆盖现有文件，并核对原内容摘要。');
        if (typeof scope.content === 'string') scopeLine('写入内容', scope.content, 600);
        if (typeof scope.old === 'string') scopeLine('替换原文', scope.old, 400);
        if (typeof scope.new === 'string') scopeLine('替换为', scope.new, 400);
      }
      scopeLine('授权范围', '仅限本次列明的能力和完全相同的参数；允许此任务也不会扩展到其他文件或内容。');
      const details = el('details');
      details.append(el('summary', '', '查看完整授权参数'), el('pre', '', recordText(approval)));
      card.append(details);
      if (typeof id !== 'string') {
        card.append(el('p', 'error', '审批记录缺少 ID，请在宿主中处理。'));
      } else {
        const actions = el('div', 'approval-actions');
        for (const [decision, label, className] of [['allow_once', '允许一次', ''], ['allow_task', '允许此任务', 'secondary'], ['deny', '拒绝', 'secondary deny']]) {
          const button = el('button', className, approvalPending.has(`${snapshot.id}/${id}`) ? '正在记录…' : label);
          button.type = 'button';
          const view = views.get(snapshot.id);
          button.disabled = approvalPending.has(`${snapshot.id}/${id}`) || unresolved(snapshot) || frozen(snapshot, view) || Boolean(view?.faults.reconciliation) || view?.writes.some(write => write.phase === 'uncertain' || write.method === 'approval.decide');
          button.addEventListener('click', () => decide(snapshot.id, id, decision));
          actions.append(button);
        }
        card.append(actions);
      }
      fragment.append(card);
    }
    ui['approvals'].replaceChildren(fragment);
    ui['approvals'].hidden = !approvals.length;
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
      renderEvents(view);
      ui['live-text'].textContent = view.liveText;
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
      const snapshot = await rpc('session.get', { session_id: id });
      if (login !== loginGeneration || generation !== selectionGeneration) return;
      rememberView(applySnapshot(views.get(id), snapshot, false, requestedAfter, requestedEpoch));
      updateSessionSummary(snapshot);
      renderView();
      pollFailures = 0;
      ui['connection-status'].textContent = '已连接本机';
      ui['poll-status'].textContent = '已同步';
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
  async function writeRpc(id, method, params, budgetProposal = null) {
    const scope = sessionWorkspaces.get(id) || workspaceId;
    sessionWorkspaces.set(id, scope);
    const login = loginGeneration;
    const key = ++writeSequence;
    const view = views.get(id);
    let admittedTaskId = null;
    let resumeStarted = false;
    if (view) rememberView(beginWrite(view, key, method, params));
    if (id === selected) renderView();
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
        taskAdmissions.set(id, admission);
        if (id === selected) renderComposer();
        const taskId = await admission;
        admittedTaskId = taskId;
        if (budgetProposal) {
          await rpc('model.budget.continue', {
            session_id: id,
            task_id: taskId,
            expected: budgetProposal.expected,
            additional_tokens: budgetProposal.additional_tokens,
            token_limit: budgetProposal.token_limit,
            confirm: true,
          }, scope);
        }
        resumeStarted = true;
        result = await rpc('task.resume', { task_id: taskId }, scope);
      } else result = await rpc(method, params, scope);
      if (login === loginGeneration && views.has(id)) rememberView(finishWrite(views.get(id), key, null, result));
      return result;
    } catch (error) {
      if (admittedTaskId && !resumeStarted) {
        const grantError = error;
        try {
          const task = await rpc('task.get', { task_id: admittedTaskId }, scope);
          const target = unstartedContinuationCancelTarget(task, id, admittedTaskId);
          const cancelled = await rpc('session.cancel', target, scope);
          if (cancelled?.cancelled !== true) throw new Error('Runtime 未确认取消从未领取的新任务。');
          error = new RpcError(
            'budget_continuation_not_executed',
            `模型预算授权失败；新任务已在从未领取的 queued 状态下取消，模型请求未开始：${grantError.message || String(grantError)}`,
            false
          );
        } catch (cleanup) {
          error = new RpcError(
            'budget_continuation_cleanup_unknown',
            `模型预算授权失败后，无法确认新任务是否已被领取或取消：${cleanup.message || String(cleanup)}`,
            true
          );
        }
      }
      if (login === loginGeneration && views.has(id)) rememberView(finishWrite(views.get(id), key, error));
      throw error;
    } finally {
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
    updateSessionSummary(snapshot);
    if (id === selected) renderView();
  }

  async function decide(id, approvalId, decision) {
    const key = `${id}/${approvalId}`;
    if (approvalPending.has(key)) return;
    const login = loginGeneration;
    approvalPending.add(key);
    report(null);
    renderApprovals(activeView()?.snapshot);
    try {
      await writeRpc(id, 'approval.decide', { session_id: id, approval_id: approvalId, decision });
      if (login !== loginGeneration) return;
      await freshSnapshot(id, login);
    } catch (error) {
      if (login === loginGeneration) handleError(error);
    } finally {
      if (login === loginGeneration) {
        approvalPending.delete(key);
        renderApprovals(activeView()?.snapshot);
        renderComposer();
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
    ui['login-button'].textContent = '连接中…';
    ui['login-error'].textContent = '';
    const login = ++loginGeneration;
    try {
      await activateLaunchWorkspace();
      const health = await rpc('runtime.health');
      if (login !== loginGeneration) return;
      if (health?.status !== 'ok' || health?.protocol_version !== CORE_PROTOCOL_VERSION) throw new Error('本机运行服务的版本不匹配，请同步更新运行服务和网页界面。');
      connected = true;
      ui['login-panel'].hidden = true;
      ui['workspace'].hidden = false;
      ui['logout'].hidden = !loginRequired;
      ui['connection-status'].textContent = '已连接本机';
      report(null);
      renderView();
      await refreshWorkspaceCatalog();
      await refreshOptionalTools();
      await refreshPanels();
      await refreshSessions();
      if (login === loginGeneration && sessions.length) await selectSession(sessions[0].id);
      else ui['session-label'].focus();
    } catch (error) {
      if (login !== loginGeneration) return;
      disconnect(errorText(error));
    } finally {
      ui['login-button'].disabled = false;
      ui['login-button'].textContent = '连接';
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
      status.textContent = '所有工作区都通过唯一 Client 打开。新目录使用 cayw init --workspace DIR 登记后刷新；切换不停止旧任务。';
    } catch (error) { $('switch-workspace').disabled = true; status.textContent = `工作区目录不可用，请确认唯一 Client 正在运行。${error.message || String(error)}`; }
  }
  async function refreshOptionalTools() {
    const container = $('optional-tools-status');
    try {
      const tools = await rpc('settings.tools');
      if (!object(tools?.editor) || !object(tools?.trace) || !object(tools?.pi)) throw new Error('宿主未提供可选工具状态；连接外部服务前请先确认已安装并启动。');
      toolAvailability = tools; container.replaceChildren(el('p', '', tools.note || '请在 CAYW 源码根目录运行安装命令，并单独启动外部服务。'));
      for (const [name, label] of [['editor','可视化编辑器'],['trace','Trace 调试'],['pi','Pi 兼容']]) {
        const item = tools[name]; const state = !item.installed ? '未安装 / 未编译' : name === 'pi' ? (item.enabled ? '已启用' : '未启用') : '已安装，需单独启动并连接服务';
        container.append(el('p', '', `${label}：${state}`), el('pre', '', `${item.install}\n${item.start}`));
        const button = $('wb-' + name);
        if (button) { button.disabled = !item.installed; button.textContent = `${label}（${item.installed ? '需连接' : '未安装'}）`; button.title = `${state}。${item.install}；${item.start}`; }
      }
    } catch (error) {
      toolAvailability = {};
      for (const name of ['editor','trace']) { const button = $('wb-' + name); if (button) { button.disabled = true; button.textContent = `${name === 'editor' ? '可视化编辑器' : 'Trace 调试'}（状态未确认）`; } }
      container.textContent = `可选工具状态未确认，请确认服务与界面版本一致；安装与启动说明见本地工作流文档。${error.message || String(error)}`;
    }
  }
  async function switchWorkspace() {
    if (workspaceBusy || !connected) return;
    workspaceBusy = true; $('switch-workspace').disabled = true;
    const login = loginGeneration;
    const status = $('workspace-switch-status');
    status.textContent = '正在通过唯一 Client 打开工作区；原工作区任务继续运行…';
    try {
      const target = await rpc('settings.workspaces.open', {id:$('workspace-choice').value});
      if (login !== loginGeneration) return;
      if (typeof target?.id !== 'string' || typeof target?.path !== 'string') throw new Error('工作区打开回执无效，请刷新状态。');
      if (target.id !== workspaceId) {
        // In-flight enqueue/resume keeps its captured workspace and transport.
        // Do not disconnect, abort controllers, or cancel another root's task.
        workspaceId = target.id;
        ++selectionGeneration; ++listGeneration; clearTimeout(timer);
        selected = null; sessions = []; openedPanels = []; openedPanelsSession = null;
        ui['fork-dialog'].close(); setPromptValue(''); ui['session-picker'].open = true;
        report(null); renderSessionList(); renderView(); renderOpenedPanels();
        await refreshPanels(); await refreshSessions();
        if (sessions.length) await selectSession(sessions[0].id);
      }
      $('workspace-choice').value = target.id;
      window.history?.replaceState(null, '', `#workspace=${target.id}`);
      await refreshOptionalTools();
      status.textContent = `当前：${target.path}。${target.reused ? '已复用 Client 内现有 Runtime。' : '已由唯一 Client 打开 Runtime。'}旧工作区任务继续运行。`;
    } catch (error) { status.textContent = errorText(error); }
    finally { workspaceBusy = false; $('switch-workspace').disabled = false; }
  }
  $('switch-workspace').addEventListener('click', switchWorkspace);
  $('refresh-workspaces').addEventListener('click', refreshWorkspaceCatalog);
  $('refresh-optional-tools').addEventListener('click', refreshOptionalTools);

  mountSettings(rpc);
  ui['logout'].addEventListener('click', async () => {
    try {
      const response = await fetch('/api/auth/logout', { method:'POST', credentials:'same-origin' });
      if (!response.ok) throw new Error('无法注销本机会话，请重试。');
      disconnect('已退出，请重新登录。');
    } catch (error) { report(error); }
  });
  ui['refresh-sessions'].addEventListener('click', refreshSessions);
  ui['refresh-panels'].addEventListener('click', refreshPanels);
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
      const deviceVal = ui['session-device']?.value?.trim();
      if (deviceVal && deviceVal !== 'local') params.device_id = deviceVal;
      const snapshot = await rpc('session.create', params);
      sessionWorkspaces.set(snapshot.id, scope);
      if (login !== loginGeneration || scope !== workspaceId) return;
      const view = applySnapshot(createView(snapshot.id), snapshot, true);
      rememberView(view);
      ui['session-label'].value = '';
      if (ui['session-device']) ui['session-device'].value = '';
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
    promptRevision++;
    renderComposer();
  });
  ui['prompt'].addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.isComposing && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      if (!ui['send-button'].disabled) ui['prompt-form'].requestSubmit();
    }
  });
  ui['prompt-form'].addEventListener('submit', async event => {
    event.preventDefault();
    if (ui['send-button'].disabled || !selected) return;
    const id = selected;
    const draft = ui['prompt'].value;
    const draftRevision = promptRevision;
    const text = draft.trim();
    const login = loginGeneration;
    const scope = workspaceId;
    const requestedAfter = views.get(id)?.cursor ?? 0;
    const requestedEpoch = views.get(id)?.proofEpoch ?? 0;
    promptPending.add(id);
    report(null);
    renderComposer();
    try {
      const budget = await rpc('model.budget.status', { session_id: id }, scope);
      let budgetProposal = null;
      if (budget?.budget_unknown) {
        budgetProposal = await confirmModelBudget(budget);
        if (!budgetProposal || login !== loginGeneration || id !== selected || scope !== workspaceId) return;
      }
      // Never await prompt from inside the polling loop. Approval/cancel/events
      // need separate concurrent HTTP and IPC requests while it is pending.
      const snapshot = await writeRpc(id, 'session.prompt', { session_id: id, text }, budgetProposal);
      if (login !== loginGeneration) return;
      const view = views.get(id);
      if (view) rememberView({ ...applySnapshot(view, snapshot, false, requestedAfter, requestedEpoch), liveText: '' });
      updateSessionSummary(snapshot);
      if (id === selected && scope === workspaceId && promptRevision === draftRevision && ui['prompt'].value === draft) setPromptValue('');
      if (id === selected) renderView();
    } catch (error) {
      if (login !== loginGeneration) return;
      if (scope === workspaceId) handleError(error);
    } finally {
      if (login === loginGeneration) {
        promptPending.delete(id);
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
      // Capture the original admission before any await. A replacement prompt
      // in the same Session must never become this stop action's target.
      const admission = taskAdmissions.get(id);
      const taskId = admission ? await admission : views.get(id)?.originalTaskId;
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
      ui['cancel-button'].textContent = '停止运行';
      renderComposer();
    }
  });

  ui['open-fork'].addEventListener('click', () => {
    if (!canFork(activeView())) return;
    ui['fork-label'].value = `${activeView()?.snapshot?.label ?? '会话'} · 普通聊天`.slice(0, 160);
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
    ui['fork-button'].disabled = true;
    ui['fork-button'].textContent = '正在创建…';
    ui['fork-error'].textContent = '';
    try {
      const snapshot = await writeRpc(id, 'session.fork', { session_id: id, label: ui['fork-label'].value.trim(), confirm: true, mode: 'chat' });
      // Validate the operation receipt before clearing its source lock. A
      // malformed success may already have created a session and cannot retry.
      if (typeof snapshot?.id !== 'string' || !snapshot.id || snapshot.id === id) throw new RpcError('invalid_response', '新会话结果不完整，请由宿主核对；不要再次 Fork。', true);
      const forkView = applySnapshot(createView(snapshot.id), snapshot, true);
      const source = views.get(id);
      if (source && login === loginGeneration) rememberView({ ...source, writes: source.writes.filter(write => write.method !== 'session.fork') });
      if (login !== loginGeneration) return;
      rememberView(forkView);
      updateSessionSummary(snapshot);
      ui['fork-dialog'].close();
      await selectSession(snapshot.id);
    } catch (error) {
      if (login === loginGeneration) {
        const source = views.get(id);
        if (source) rememberView({ ...source, writes:source.writes.map(write => write.method === 'session.fork' ? { ...write,phase:'uncertain' } : write) });
        renderView();
        if (error.auth) disconnect(error.message);
        else ui['fork-error'].textContent = errorText(error);
      }
    } finally {
      ui['fork-button'].disabled = false;
      ui['fork-button'].textContent = '确认 Fork';
    }
  });

  ui['refresh-capabilities'].addEventListener('click', async () => {
    const login = loginGeneration;
    ui['refresh-capabilities'].disabled = true;
    ui['capability-content'].textContent = '正在读取…';
    try {
      const capabilities = await rpc('capabilities.list');
      if (login === loginGeneration) ui['capability-content'].textContent = recordText(capabilities);
    } catch (error) {
      if (login === loginGeneration) {
        ui['capability-content'].textContent = errorText(error);
        if (error.auth) disconnect(error.message);
      }
    } finally { ui['refresh-capabilities'].disabled = false; }
  });

  const setupWbButton = (btnId, panelId, zone = 'sidebar') => {
    const button = ui[btnId];
    if (!button) return;
    button.addEventListener('click', async () => {
      const currentSession = panelSessionId();
      if (openedPanelsSession !== currentSession) {
        openedPanels = [];
        openedPanelsSession = currentSession;
        renderOpenedPanels();
      }
      const existing = openedPanels.find(p => p.panel_id === panelId && (!p.session_id || p.session_id === currentSession));
      if (existing) {
        await activateOpenedPanel(existing.instance_id);
      } else {
        await openRegisteredPanel(panelId, zone);
      }
    });
  };
  setupWbButton('wb-tasks', 'tasks', 'sidebar');
  setupWbButton('wb-usage', 'usage', 'sidebar');
  setupWbButton('wb-artifacts', 'artifacts', 'sidebar');
  setupWbButton('wb-plannotator', 'plannotator', 'sidebar');
  setupWbButton('wb-editor', 'editor', 'sidebar');
  setupWbButton('wb-trace', 'trace', 'sidebar');

  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && connected && selected) schedulePoll(0);
  });
  window.addEventListener('online', () => { if (connected && selected) schedulePoll(0); });
  window.addEventListener('pagehide', () => { ui.password.value = ''; for (const controller of controllers) controller.abort(); });
}

// Host settings share the existing authenticated RPC and its cancellation/error
// handling. Neither credential values nor plugin code are stored by the browser.
function mountSettings(rpc) {
  const $ = id => document.getElementById(id);
  const open = $('open-settings');
  if (!open) return;
  const dialog = $('settings-dialog');
  let busy = false;
  const status = text => { $('settings-status').textContent = text; };
  const button = (text, handler) => {
    const node = document.createElement('button'); node.type = 'button'; node.textContent = text;
    node.addEventListener('click', () => action(handler)); return node;
  };
  async function action(handler) {
    if (busy) return;
    busy = true; dialog.setAttribute('aria-busy', 'true'); status('处理中…');
    try { await handler(); status('已完成。'); }
    catch (error) { status(error.message || String(error)); }
    finally { busy = false; dialog.removeAttribute('aria-busy'); }
  }
  let accountRevision = null;
  let accountCanManage = false;
  let accountBusy = false;
  const clearAccountPasswords = () => { $('account-password').value = ''; $('account-confirm').value = ''; };
  function accountControls() {
    for (const id of ['account-username','account-password','account-confirm','save-account']) $(id).disabled = !accountCanManage || accountBusy;
  }
  function showAccount(state) {
    accountRevision = state.revision ?? null; accountCanManage = state.can_manage === true;
    if (!$('account-username').value || !accountBusy) $('account-username').value = state.username || '';
    $('account-settings-status').textContent = state.message || '账号状态已读取。';
    renderLanNotice(state.lan_active === true && state.login_required !== true);
    accountControls();
  }
  async function refreshAccount() {
    try {
      const response = await fetch('/api/settings/account', {credentials:'same-origin',cache:'no-store'});
      const state = await response.json();
      if (!response.ok) throw new Error(state.error?.message || '无法读取账号状态');
      showAccount(state);
    } catch (error) { accountCanManage = false; accountControls(); $('account-settings-status').textContent = error.message || '账号状态未确认，请刷新重试。'; }
  }
  $('account-settings-form').addEventListener('submit', async event => {
    event.preventDefault(); if (accountBusy || !accountCanManage) return;
    const values = {username:$('account-username').value,password:$('account-password').value,confirm_password:$('account-confirm').value,expected_revision:accountRevision};
    clearAccountPasswords();
    if (values.password !== values.confirm_password) { $('account-settings-status').textContent = '两次密码不一致，未保存。'; return; }
    accountBusy = true; accountControls(); $('account-settings-status').textContent = '正在保存账号…';
    try {
      const response = await fetch('/api/settings/account', {method:'POST',credentials:'same-origin',cache:'no-store',headers:{'Content-Type':'application/json'},body:JSON.stringify(values)});
      const state = await response.json();
      if (!response.ok) { if (state.error?.outcome_unknown) accountCanManage = false; throw new Error(state.error?.message || '保存未完成，请刷新账号状态核对。'); }
      showAccount(state);
      // The server's current status distinguishes immediate LAN activation
      // from a local-only gateway saving an account for its next LAN launch.
    } catch (error) { const unconfirmed = error instanceof TypeError || error instanceof SyntaxError; if (unconfirmed) accountCanManage = false; $('account-settings-status').textContent = unconfirmed ? '保存回执未确认，请刷新账号状态核对；不会自动重试。' : error.message; }
    finally { accountBusy = false; accountControls(); clearAccountPasswords(); }
  });
  $('refresh-account').addEventListener('click', () => { if (!accountBusy) refreshAccount(); });
  dialog.addEventListener('close', clearAccountPasswords);
  window.addEventListener('pagehide', clearAccountPasswords);
  const modelForm = $('model-settings-form');
  function showModelApiKeyState(hasApiKey) {
    modelForm.elements.clear_api_key.checked = false;
    modelForm.elements.clear_api_key.disabled = !hasApiKey;
    modelForm.elements.api_key.placeholder = hasApiKey
      ? '已保存密钥；留空表示保持不变'
      : '无认证服务请留空';
    $('model-api-key-status').textContent = hasApiKey
      ? '此方案已有密钥。留空会保留原值；只有勾选下方选项才会清除。'
      : '此方案没有已保存密钥。';
  }
  const serviceDefaults = {
    openai: ['https://api.openai.com/v1', ['chat_completions', 'responses']],
    anthropic: ['https://api.anthropic.com/v1', ['messages']],
    gemini: ['https://generativelanguage.googleapis.com/v1beta', ['stream_generate_content', 'generate_content']],
    magpie: ['http://127.0.0.1:3425/v1', ['chat_completions', 'responses', 'messages', 'stream_generate_content', 'generate_content']],
  };
  function serviceOptions(service, defaults) {
    const [base, apis] = serviceDefaults[service];
    modelForm.elements.api.replaceChildren(...apis.map(api => { const option = document.createElement('option'); option.value = api; option.textContent = api; return option; }));
    if (defaults) { modelForm.elements.base_url.value = base; modelForm.elements.api_key.value = ''; apiVersion(); }
  }
  function apiVersion() {
    const api = modelForm.elements.api.value;
    modelForm.elements.api_version.value = api === 'messages' ? '2023-06-01' : api.includes('generate_content') ? 'v1beta' : 'v1';
    if (modelForm.elements.service.value === 'magpie' && /^http:\/\/127\.0\.0\.1:3425\/v1(?:beta)?$/.test(modelForm.elements.base_url.value)) modelForm.elements.base_url.value = api.includes('generate_content') ? 'http://127.0.0.1:3425/v1beta' : 'http://127.0.0.1:3425/v1';
  }
  if (modelForm.elements) {
    showModelApiKeyState(false);
    modelForm.elements.service.addEventListener('change', () => serviceOptions(modelForm.elements.service.value, true));
    modelForm.elements.api.addEventListener('change', apiVersion);
    modelForm.elements.api_key.addEventListener('input', () => {
      if (modelForm.elements.api_key.value) modelForm.elements.clear_api_key.checked = false;
    });
    modelForm.elements.clear_api_key.addEventListener('change', () => {
      if (modelForm.elements.clear_api_key.checked) modelForm.elements.api_key.value = '';
    });
  }

  async function refresh() {
    await refreshAccount();
    const workspace = await rpc('settings.workspace');
    $('settings-workspace').textContent = `当前目录：${workspace.workspace}${workspace.offline ? ' · 离线演示' : ''}\n已登记工作区：${(workspace.known_workspaces || []).join('、')}`;
    const models = await rpc('settings.models.list');
    const modelList = $('model-profiles'); modelList.replaceChildren();
    const current = document.createElement('p'); current.textContent = `当前：${models.health.provider} / ${models.health.model}`; modelList.append(current);
    for (const [name, model] of Object.entries(models.models || {})) {
      const row = document.createElement('div'); const label = document.createElement('p');
      label.textContent = `${name} · ${model.id}${models.selected === name ? ' · 已保存选择' : ''}`; row.append(label);
      row.append(button('编辑', async () => {
        modelForm.reset();
        modelForm.elements.name.value = name;
        serviceOptions(model.service, false);
        modelForm.elements.api_key.value = '';
        for (const [key, value] of Object.entries(model)) {
          if (modelForm.elements[key]) modelForm.elements[key].value = value;
        }
        showModelApiKeyState(model.authentication === 'plain_file');
      }));
      const use = button('使用', async () => { await rpc('settings.models.use', {name}); await refresh(); }); use.disabled = models.offline; row.append(use);
      const remove = button('删除', async () => { await rpc('settings.models.remove', {name}); await refresh(); }); remove.disabled = models.selected === name; row.append(remove); modelList.append(row);
    }
    const list = $('plugin-profiles'); list.replaceChildren();
    try {
      const plugins = await rpc('settings.plugins.list');
      for (const plugin of plugins.entries || []) {
        const row = document.createElement('div'); const label = document.createElement('p');
        label.textContent = `${plugin.name} ${plugin.descriptor.manifest.version} · ${plugin.available ? '可用' : plugin.enabled ? '异常' : '已停用'}${plugin.last_error ? ' · ' + plugin.last_error : ''}`; row.append(label);
        const detail = document.createElement('pre'); detail.textContent = JSON.stringify({ ...plugin.descriptor, executable_sha256: plugin.executable_sha256 }, null, 2); row.append(detail);
        row.append(button('编辑登记', async () => { $('plugin-settings-form').elements.name.value = plugin.name; $('plugin-settings-form').elements.descriptor.value = JSON.stringify(plugin.descriptor, null, 2); }));
        const loaded = plugin.enabled || plugin.available;
        row.append(button(loaded ? '停用' : '启用', async () => {
          const approve = $('plugin-process-approval');
          if (!loaded && !approve.checked) throw new Error('请先审阅插件并勾选本地进程执行批准。');
          await rpc(`settings.plugins.${loaded ? 'disable' : 'enable'}`, { name: plugin.name, ...(!loaded ? {approve_process:true} : {}) });
          approve.checked = false; await refresh();
        }));
        row.append(button('卸载登记', async () => { await rpc('settings.plugins.remove', {name:plugin.name}); await refresh(); })); list.append(row);
      }
    } catch (error) { const message = document.createElement('p'); message.textContent = error.message || String(error); list.append(message); }
  }
  open.addEventListener('click', () => { $('plugin-process-approval').checked = false; dialog.showModal(); action(refresh); });
  $('close-settings').addEventListener('click', () => dialog.close());
  $('refresh-settings').addEventListener('click', () => action(refresh));
  modelForm.addEventListener('submit', event => {
    event.preventDefault(); action(async () => {
      const values = Object.fromEntries(new FormData(modelForm)); const name = values.name; delete values.name;
      const clearApiKey = values.clear_api_key === 'on'; delete values.clear_api_key;
      values.max_output_tokens = Number(values.max_output_tokens);
      if (clearApiKey && values.api_key) throw new Error('不能同时填写新密钥和清除已保存密钥。');
      if (!values.api_key) delete values.api_key;
      await rpc('settings.models.put', {name, model:values, clear_api_key:clearApiKey}); await refresh();
    });
  });
  $('plugin-settings-form').addEventListener('submit', event => {
    event.preventDefault(); action(async () => {
      const form = $('plugin-settings-form'); const name = form.elements.name.value.trim();
      await rpc(`settings.plugins.${name ? 'update' : 'install'}`, { ...(name ? {name} : {}), descriptor:JSON.parse(form.elements.descriptor.value) }); await refresh();
    });
  });
}
