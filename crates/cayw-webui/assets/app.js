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
const readMethods = new Set(['runtime.health', 'projects.list', 'session.list', 'session.get', 'session.events', 'capabilities.list']);
const rejectedCodes = new Set(['forbidden', 'invalid_params', 'invalid_request', 'invalid_json', 'method_not_allowed', 'queue_full', 'queue_timeout', 'body_timeout', 'invalid_body', 'busy', 'session_frozen', 'needs_reconciliation', 'stale_approval', 'cancelled']);

export class RpcError extends Error {
  constructor(code, message, outcomeUnknown = false, status = 0) {
    super(clip(message, 2000));
    this.code = code;
    this.outcome_unknown = outcomeUnknown;
    this.status = status;
    this.auth = code === 'forbidden' && status === 403;
  }
}

export function beginWrite(view, key, method, params = {}) {
  const after = Math.max(view.cursor, view.snapshot?.cursor ?? 0);
  // A prompt has no accepted task identity until its own HTTP receipt returns.
  // The session's current/next attempt may belong to an entirely different client.
  const lease = method === 'session.prompt' ? null : view.snapshot?.attempt ?? view.activeAttempt?.lease ?? null;
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
  if ((error.code === 'needs_reconciliation' || error.code === 'tool_outcome_unknown') && !faults.reconciliation) faults.reconciliation = { after, callIds: [], terminal: null };
  return { ...view, faults };
}

export function finishWrite(view, key, error = null, result = null) {
  const rejected = error && !error.outcome_unknown && rejectedCodes.has(error.code);
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

export function summarizeProjects(projects = [], projectSessions = {}, projectViews = {}) {
  const list = Array.isArray(projects) && projects.length
    ? projects.filter(item => object(item) && typeof item.id === 'string' && item.id)
    : [{ id: 'default', label: '默认项目', address: '本机 IPC', default: true }];
  return list.slice(0, 32).map((project, index) => {
    const id = clip(project.id, 64);
    const sessions = array(projectSessions[id]);
    const views = projectViews[id] instanceof Map ? projectViews[id] : new Map(Object.entries(projectViews[id] ?? {}));
    let runningCount = 0;
    let approvalCount = 0;
    let frozenCount = 0;
    let unresolvedCount = 0;
    let uncertainCount = 0;
    for (const session of sessions) {
      const view = views.get(session?.id);
      const snap = view?.snapshot ?? session;
      if (unresolved(snap) || view?.faults?.reconciliation) unresolvedCount++;
      if (view?.writes?.some(write => write.phase === 'uncertain')) uncertainCount++;
      if (frozen(snap, view)) frozenCount++;
      if (array(snap?.pending_approvals).length) approvalCount++;
      if (sessionRunning(snap)) runningCount++;
    }
    for (const [sessionId, view] of views.entries()) {
      if (sessions.some(session => session?.id === sessionId)) continue;
      if (view?.faults?.reconciliation) unresolvedCount++;
      if (view?.writes?.some(write => write.phase === 'uncertain')) uncertainCount++;
      if (view?.faults?.frozen) frozenCount++;
    }
    const statusLabel = unresolvedCount
      ? `${unresolvedCount} 个会话待核对副作用`
      : uncertainCount
        ? `${uncertainCount} 个会话连接结果待确认`
        : approvalCount
          ? `${approvalCount} 个会话等待审批`
          : runningCount
            ? `${runningCount} 个会话运行中`
            : frozenCount
              ? `${frozenCount} 个会话已冻结`
              : '就绪';
    return {
      id,
      label: clip(project.label || id, 160),
      address: clip(project.address || '本机 loopback IPC', 120),
      default: Boolean(project.default ?? index === 0),
      sessionCount: sessions.length,
      runningCount,
      approvalCount,
      frozenCount,
      unresolvedCount,
      uncertainCount,
      statusLabel,
    };
  });
}

const hostCapabilityNamespaces = new Set([
  'fs', 'search', 'code', 'data', 'git', 'build', 'test',
  'exec_command', 'write_stdin', 'mcp', 'skills',
  'archive', 'batch', 'codec', 'csv', 'diff', 'hash', 'text',
  'json', 'query', 'toml', 'xml', 'yaml', 'workspace', 'process', 'request', 'task',
]);

export function summarizePlugins(sessions = [], capabilities = [], views = new Map()) {
  const viewMap = views instanceof Map ? views : new Map(Object.entries(views ?? {}));
  const byName = new Map();
  for (const session of array(sessions)) {
    if (!object(session) || typeof session.id !== 'string') continue;
    const view = viewMap.get(session.id);
    const snap = view?.snapshot ?? session;
    const binding = object(snap?.binding) ? snap.binding : null;
    const isFrozen = frozen(snap, view);
    if (!binding && !isFrozen) continue;
    const name = clip(binding?.plugin || '未知插件绑定', 120);
    const entry = byName.get(name) ?? {
      name,
      version: clip(binding?.version || binding?.manifest?.version || '未标注版本', 64),
      registryId: clip(binding?.registry_id || '未记录实例 ID', 80),
      generation: safeCursor(binding?.generation) ? binding.generation : null,
      frozen: false,
      reasons: [],
      capabilities: [],
      boundSessions: [],
    };
    if (isFrozen) entry.frozen = true;
    if (typeof binding?.reason === 'string' && binding.reason.trim() && !entry.reasons.includes(binding.reason.trim())) {
      entry.reasons.push(clip(binding.reason.trim(), 400));
    }
    for (const cap of array(binding?.manifest?.capabilities)) {
      const capName = typeof cap?.name === 'string' ? clip(cap.name, 120) : '';
      if (capName && !entry.capabilities.includes(capName)) entry.capabilities.push(capName);
    }
    const candidateView = view ?? { snapshot: snap, faults: {}, writes: [], activeAttempt: null };
    entry.boundSessions.push({
      id: session.id,
      label: clip(snap?.label || session.label || '未命名会话', 160),
      stateLabel: stateLabel(snap, view),
      frozen: isFrozen,
      needsReconciliation: Boolean(unresolved(snap) || view?.faults?.reconciliation),
      canFork: canFork(candidateView),
    });
    byName.set(name, entry);
  }
  for (const cap of array(capabilities)) {
    if (!object(cap) || typeof cap.name !== 'string') continue;
    const prefix = cap.name.includes('.') ? cap.name.split('.')[0] : '';
    if (!prefix || hostCapabilityNamespaces.has(prefix)) continue;
    const entry = byName.get(prefix) ?? {
      name: clip(prefix, 120),
      version: '宿主当前目录',
      registryId: '宿主公开能力',
      generation: null,
      frozen: false,
      reasons: [],
      capabilities: [],
      boundSessions: [],
    };
    const capName = clip(cap.name, 120);
    if (!entry.capabilities.includes(capName)) entry.capabilities.push(capName);
    byName.set(prefix, entry);
  }
  return [...byName.values()].map(entry => ({
    ...entry,
    statusLabel: entry.frozen ? '已冻结 · 不可用' : '正常启用',
  }));
}

if (typeof document !== 'undefined') boot();

function boot() {
  const $ = id => document.getElementById(id);
  const ui = Object.fromEntries([
    'login-panel', 'login-form', 'login-error', 'login-button', 'web-token', 'workspace', 'logout', 'connection-status',
    'nav-conversation', 'nav-projects', 'nav-plugins', 'conversation-view', 'projects-page', 'plugins-page',
    'refresh-projects', 'projects-status', 'project-list', 'refresh-plugins', 'plugins-status', 'plugin-list',
    'create-form', 'session-label', 'create-button', 'refresh-sessions', 'list-status', 'session-list', 'session-picker', 'session-picker-label',
    'conversation', 'session-title', 'session-state', 'global-error', 'poll-status', 'messages', 'history-note',
    'blocked-notice', 'blocked-text', 'open-fork', 'approvals', 'live-section', 'live-text', 'event-log', 'event-count',
    'prompt-form', 'prompt', 'prompt-status', 'send-button', 'cancel-button', 'fork-dialog', 'fork-form',
    'fork-label', 'fork-confirm', 'fork-error', 'fork-button', 'close-fork', 'refresh-capabilities', 'capability-content',
  ].map(id => [id, $(id)]));
  let token = '';
  let loginGeneration = 0;
  let selected = null;
  let sessions = [];
  let views = new Map();
  let promptPending = new Set();
  let approvalPending = new Set();
  let activePage = 'conversation';
  let activeProject = null;
  let projects = [{ id: 'default', label: '默认项目', address: '本机 IPC', default: true }];
  let projectStates = new Map();
  let cachedCapabilities = [];
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
    session_frozen: '强流程当前不可用，会话保持冻结。请在会话提示中确认后创建普通聊天分支。',
    needs_reconciliation: '工具副作用尚未核对，需要由宿主检查真实结果。',
    tool_outcome_unknown: '工具结果尚未核对，需要由宿主检查真实副作用。',
  })[error?.code] ?? (error?.outcome_unknown ? '连接中断或请求结果待确认。页面不会自动重发，请等待事件同步或由宿主核对。' : error?.message ?? '发生未知错误。'), 2500);
  const report = error => {
    ui['global-error'].textContent = error ? errorText(error) : '';
    ui['global-error'].hidden = !error;
  };
  const activeView = () => selected ? views.get(selected) : undefined;
  const currentProjectId = () => activeProject ?? projects[0]?.id ?? 'default';

  function saveProjectState() {
    projectStates.set(currentProjectId(), {
      selected,
      sessions,
      views,
      promptPending,
      approvalPending,
      cachedCapabilities,
    });
  }

  function loadProjectState(projectId) {
    const state = projectStates.get(projectId);
    selected = state?.selected ?? null;
    sessions = state?.sessions ?? [];
    views = state?.views ?? new Map();
    promptPending = state?.promptPending ?? new Set();
    approvalPending = state?.approvalPending ?? new Set();
    cachedCapabilities = state?.cachedCapabilities ?? [];
  }

  async function rpc(method, params = {}) {
    const controller = new AbortController();
    controllers.add(controller);
    const requestToken = token;
    const requestBody = activeProject ? { method, params, project: activeProject } : { method, params };
    let timeout;
    if (method !== 'session.prompt') timeout = setTimeout(() => controller.abort(), 15000);
    try {
      const response = await fetch('/api/rpc', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${requestToken}` },
        body: JSON.stringify(requestBody),
        signal: controller.signal,
        cache: 'no-store',
        credentials: 'omit',
      });
      let payload;
      try { payload = await response.json(); } catch { throw new RpcError('invalid_response', '网关返回了无效 JSON；当前请求不会自动重发。', !readMethods.has(method), response.status); }
      if (!response.ok || payload?.error) {
        const detail = payload?.error;
        throw new RpcError(typeof detail?.code === 'string' ? detail.code : 'http_error', detail?.code === 'forbidden' && response.status === 403 ? '网页访问令牌无效或已失效，请重新连接。' : detail?.message ?? `请求失败（HTTP ${response.status}）。`, typeof detail?.outcome_unknown === 'boolean' ? detail.outcome_unknown : !readMethods.has(method), response.status);
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

  function disconnect(reason = '') {
    saveProjectState();
    loginGeneration++;
    selectionGeneration++;
    listGeneration++;
    clearTimeout(timer);
    for (const controller of controllers) controller.abort();
    controllers.clear();
    token = '';
    selected = null;
    sessions = [];
    // Keep unresolved operation evidence in this page's memory across
    // reauthentication. Aborting a connection never proves execution stopped.
    for (const [projectId, state] of projectStates.entries()) {
      const retained = new Map([...state.views].filter(([,view]) => view.writes.length || view.activeAttempt || Object.keys(view.faults).length).map(([id,view]) => [id,{ ...view, writes:view.writes.map(write => ({ ...write, phase:'uncertain' })) }]));
      projectStates.set(projectId, { selected: null, sessions: [], views: retained, promptPending: new Set(), approvalPending: new Set(), cachedCapabilities: [] });
    }
    views = projectStates.get(currentProjectId())?.views ?? new Map();
    promptPending = new Set();
    approvalPending = new Set();
    activePage = 'conversation';
    ui['workspace'].hidden = true;
    ui['logout'].hidden = true;
    ui['login-panel'].hidden = false;
    ui['login-error'].textContent = reason;
    ui['connection-status'].textContent = '尚未连接';
    ui['web-token'].value = '';
    ui['messages'].replaceChildren();
    ui['event-log'].replaceChildren();
    ui['approvals'].replaceChildren();
    ui['live-text'].textContent = '';
    ui['session-list'].replaceChildren();
    ui['session-picker'].open = true;
    ui['prompt'].value = '';
    if (ui['project-list']) ui['project-list'].replaceChildren();
    if (ui['plugin-list']) ui['plugin-list'].replaceChildren();
    renderPageTabs();
    ui['fork-dialog'].close();
    ui['web-token'].focus();
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
    }
  }

  function renderSessionList() {
    const fragment = document.createDocumentFragment();
    if (!sessions.length) fragment.append(el('p', 'small muted', '暂无会话。新建一个即可开始。'));
    for (const session of sessions) {
      const button = el('button', 'session-item');
      button.type = 'button';
      button.setAttribute('aria-current', String(session.id === selected));
      const shortId = String(session.id ?? '').slice(0, 8);
      button.append(el('strong', '', clip(session.label || '未命名会话', 160)), el('span', '', `${stateLabel(session, views.get(session.id))} · ${shortId}`));
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
    const current = views.get(snapshot.id)?.snapshot;
    if (current && current.cursor > snapshot.cursor) snapshot = current;
    const index = sessions.findIndex(session => session.id === snapshot.id);
    const summary = sessionSummary(snapshot);
    if (index >= 0) sessions[index] = summary;
    else sessions = [...sessions, summary].slice(-200);
    renderSessionList();
  }

  async function selectSession(id) {
    if (!token || id === selected) return;
    const generation = ++selectionGeneration;
    const login = loginGeneration;
    clearTimeout(timer);
    selected = id;
    if (!views.has(id)) rememberView(createView(id));
    ui['poll-status'].textContent = '正在读取会话…';
    ui['prompt'].value = '';
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
    renderProjectsPage();
    renderPluginsPage();
  }

  function renderPageTabs() {
    if (!ui['nav-conversation']) return;
    ui['nav-conversation'].setAttribute('aria-current', String(activePage === 'conversation'));
    ui['nav-projects'].setAttribute('aria-current', String(activePage === 'projects'));
    ui['nav-plugins'].setAttribute('aria-current', String(activePage === 'plugins'));
    if (ui['conversation-view']) ui['conversation-view'].hidden = activePage !== 'conversation';
    if (ui['projects-page']) ui['projects-page'].hidden = activePage !== 'projects';
    if (ui['plugins-page']) ui['plugins-page'].hidden = activePage !== 'plugins';
  }

  function switchPage(page) {
    activePage = page;
    renderPageTabs();
    if (page === 'projects') renderProjectsPage();
    if (page === 'plugins') renderPluginsPage();
  }

  function renderProjectsPage() {
    if (!ui['project-list']) return;
    saveProjectState();
    const sessionMap = Object.fromEntries([...projectStates.entries()].map(([id, state]) => [id, state.sessions]));
    const viewMap = Object.fromEntries([...projectStates.entries()].map(([id, state]) => [id, state.views]));
    const summaries = summarizeProjects(projects, sessionMap, viewMap);
    const fragment = document.createDocumentFragment();
    for (const item of summaries) {
      const card = el('article', 'management-card');
      const isCurrent = item.id === currentProjectId();
      if (isCurrent) card.classList.add('is-active');
      const header = el('div', 'card-header');
      header.append(
        el('strong', '', item.label),
        el('span', 'state-label', isCurrent ? '当前项目' : item.id),
      );
      const pills = el('div', 'stat-pills');
      pills.append(
        el('span', 'stat-pill', `${item.sessionCount} 个会话`),
        el('span', 'stat-pill', item.statusLabel),
        el('span', 'stat-pill mono', item.address),
      );
      if (item.frozenCount) pills.append(el('span', 'stat-pill warn', `${item.frozenCount} 已冻结`));
      if (item.unresolvedCount) pills.append(el('span', 'stat-pill danger', `${item.unresolvedCount} 待核对`));
      card.append(
        header,
        pills,
        el('p', 'small muted', `地址：${item.address} · 状态：${item.statusLabel}`),
      );
      const actions = el('div', 'approval-actions');
      const button = el('button', isCurrent ? 'secondary' : '', isCurrent ? '查看该项目会话' : '切换到此项目');
      button.type = 'button';
      button.addEventListener('click', () => selectProject(item.id));
      actions.append(button);
      card.append(actions);
      fragment.append(card);
    }
    ui['project-list'].replaceChildren(fragment);
    if (ui['projects-status']) {
      ui['projects-status'].textContent = `共 ${summaries.length} 个本机项目 · 当前项目：${summaries.find(item => item.id === currentProjectId())?.label ?? currentProjectId()}`;
    }
  }

  async function selectProject(projectId) {
    if (!token) return;
    saveProjectState();
    const isDefault = projects[0]?.id === projectId || (!projects.length && projectId === 'default');
    activeProject = isDefault ? null : projectId;
    loadProjectState(projectId);
    selectionGeneration++;
    clearTimeout(timer);
    switchPage('conversation');
    renderSessionList();
    renderView();
    await refreshSessions();
    if (sessions.length) {
      const target = selected && sessions.some(s => s.id === selected) ? selected : sessions[0].id;
      selected = null;
      await selectSession(target);
    }
  }

  async function refreshProjects() {
    if (!token || !ui['refresh-projects']) return;
    const login = loginGeneration;
    ui['refresh-projects'].disabled = true;
    ui['projects-status'].textContent = '正在读取本机项目…';
    try {
      const list = await rpc('projects.list');
      if (login !== loginGeneration) return;
      if (Array.isArray(list) && list.length) projects = list;
      await refreshSessions();
      if (login === loginGeneration) renderProjectsPage();
    } catch (error) {
      if (login === loginGeneration) {
        ui['projects-status'].textContent = errorText(error);
        handleError(error);
      }
    } finally {
      if (ui['refresh-projects']) ui['refresh-projects'].disabled = false;
    }
  }

  function renderPluginsPage() {
    if (!ui['plugin-list']) return;
    const items = summarizePlugins(sessions, cachedCapabilities, views);
    const fragment = document.createDocumentFragment();
    if (!items.length) {
      const empty = el('div', 'empty');
      empty.append(
        el('h2', '', '当前项目暂无已绑定或公开的插件'),
        el('p', '', '当会话绑定 StrongFlow 或其他 plugin，或宿主公开插件能力后，可在此查看版本、状态与关联会话。'),
      );
      fragment.append(empty);
    }
    for (const plugin of items) {
      const card = el('article', 'management-card');
      if (plugin.frozen) card.classList.add('is-frozen');
      const header = el('div', 'card-header');
      header.append(el('strong', '', plugin.name), el('span', 'state-label', plugin.statusLabel));
      const pills = el('div', 'stat-pills');
      pills.append(
        el('span', 'stat-pill', `版本 ${plugin.version}`),
        el('span', 'stat-pill mono', plugin.registryId),
      );
      if (plugin.generation !== null) pills.append(el('span', 'stat-pill', `代际 #${plugin.generation}`));
      pills.append(el('span', 'stat-pill', `${plugin.boundSessions.length} 个绑定会话`));
      card.append(
        header,
        pills,
        el('p', 'small muted', `版本：${plugin.version} · 实例：${plugin.registryId}${plugin.generation !== null ? ` · 代际 #${plugin.generation}` : ''}`),
      );
      if (plugin.reasons.length) {
        card.append(el('p', 'effect-warning', `冻结原因：${plugin.reasons.join('；')}`));
      }
      if (plugin.capabilities.length) {
        const capRow = el('div', 'stat-pills');
        for (const cap of plugin.capabilities) capRow.append(el('span', 'stat-pill mono', cap));
        card.append(el('p', 'small', `提供能力：${plugin.capabilities.join('、')}`), capRow);
      }
      for (const bound of plugin.boundSessions) {
        const row = el('div', 'bound-session-row');
        row.append(
          el('span', 'small', `${bound.label}（${bound.stateLabel}）`),
        );
        const actions = el('div', 'approval-actions');
        const openBtn = el('button', 'secondary', '打开会话');
        openBtn.type = 'button';
        openBtn.addEventListener('click', async () => {
          switchPage('conversation');
          await selectSession(bound.id);
        });
        actions.append(openBtn);
        if (bound.frozen) {
          const forkBtn = el('button', '', '确认后 Fork 为普通聊天');
          forkBtn.type = 'button';
          forkBtn.disabled = !bound.canFork;
          forkBtn.addEventListener('click', async () => {
            switchPage('conversation');
            await selectSession(bound.id);
            openForkDialog();
          });
          actions.append(forkBtn);
        }
        row.append(actions);
        card.append(row);
      }
      fragment.append(card);
    }
    ui['plugin-list'].replaceChildren(fragment);
    if (ui['plugins-status']) {
      ui['plugins-status'].textContent = items.length ? `已识别 ${items.length} 个插件绑定或能力组` : '暂无已绑定的插件记录';
    }
  }

  async function refreshPlugins() {
    if (!token || !ui['refresh-plugins']) return;
    const login = loginGeneration;
    ui['refresh-plugins'].disabled = true;
    ui['plugins-status'].textContent = '正在同步会话绑定与宿主能力…';
    try {
      await refreshSessions();
      const capabilities = await rpc('capabilities.list');
      if (login !== loginGeneration) return;
      if (Array.isArray(capabilities)) {
        cachedCapabilities = capabilities;
        if (ui['capability-content']) ui['capability-content'].textContent = recordText(capabilities);
      }
      renderPluginsPage();
    } catch (error) {
      if (login === loginGeneration) {
        ui['plugins-status'].textContent = errorText(error);
        handleError(error);
      }
    } finally {
      if (ui['refresh-plugins']) ui['refresh-plugins'].disabled = false;
    }
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
    ui['cancel-button'].disabled = !snapshot || (!busy && !view.writes.length && !view.activeAttempt && !array(snapshot.pending_approvals).length) || ui['cancel-button'].dataset.pending === 'true';
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
    if (token && selected && generation === selectionGeneration) timer = setTimeout(() => poll(generation), delay);
  }

  async function poll(generation) {
    const id = selected;
    const login = loginGeneration;
    if (!id || !token || generation !== selectionGeneration) return;
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
  async function writeRpc(id, method, params) {
    const login = loginGeneration;
    const key = ++writeSequence;
    const view = views.get(id);
    if (view) rememberView(beginWrite(view, key, method, params));
    if (id === selected) renderView();
    try {
      const result = await rpc(method, params);
      if (login === loginGeneration && views.has(id)) rememberView(finishWrite(views.get(id), key, null, result));
      return result;
    } catch (error) {
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

  ui['login-form'].addEventListener('submit', async event => {
    event.preventDefault();
    if (ui['login-button'].disabled) return;
    token = ui['web-token'].value;
    ui['web-token'].value = '';
    ui['login-button'].disabled = true;
    ui['login-button'].textContent = '连接中…';
    ui['login-error'].textContent = '';
    const login = ++loginGeneration;
    try {
      const health = await rpc('runtime.health');
      if (login !== loginGeneration) return;
      if (health?.status !== 'ok' || health?.protocol_version !== CORE_PROTOCOL_VERSION) throw new Error('本机运行服务的版本不匹配，请同步更新运行服务和网页界面。');
      ui['login-panel'].hidden = true;
      ui['workspace'].hidden = false;
      ui['logout'].hidden = false;
      ui['connection-status'].textContent = '已连接本机';
      report(null);
      renderView();
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
  });

  ui['logout'].addEventListener('click', () => disconnect());
  ui['refresh-sessions'].addEventListener('click', refreshSessions);
  ui['create-form'].addEventListener('submit', async event => {
    event.preventDefault();
    if (ui['create-button'].disabled || !ui['session-label'].value.trim()) return;
    const login = loginGeneration;
    ui['create-button'].disabled = true;
    ui['create-button'].textContent = '创建中…';
    report(null);
    try {
      const snapshot = await rpc('session.create', { label: ui['session-label'].value.trim() });
      if (login !== loginGeneration) return;
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

  ui['prompt'].addEventListener('input', renderComposer);
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
    const text = ui['prompt'].value.trim();
    const login = loginGeneration;
    const requestedAfter = views.get(id)?.cursor ?? 0;
    const requestedEpoch = views.get(id)?.proofEpoch ?? 0;
    promptPending.add(id);
    report(null);
    ui['prompt'].value = '';
    renderComposer();
    try {
      // Never await prompt from inside the polling loop. Approval/cancel/events
      // need separate concurrent HTTP and IPC requests while it is pending.
      const snapshot = await writeRpc(id, 'session.prompt', { session_id: id, text });
      if (login !== loginGeneration) return;
      const view = views.get(id);
      if (view) rememberView({ ...applySnapshot(view, snapshot, false, requestedAfter, requestedEpoch), liveText: '' });
      updateSessionSummary(snapshot);
      if (id === selected) renderView();
    } catch (error) {
      if (login !== loginGeneration) return;
      handleError(error);
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
    ui['cancel-button'].dataset.pending = 'true';
    ui['cancel-button'].textContent = '正在停止…';
    renderComposer();
    report(null);
    try {
      const result = await writeRpc(id, 'session.cancel', { session_id: id });
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

  function openForkDialog() {
    if (!canFork(activeView())) return;
    ui['fork-label'].value = `${activeView()?.snapshot?.label ?? '会话'} · 普通聊天`.slice(0, 160);
    ui['fork-confirm'].checked = false;
    ui['fork-error'].textContent = '';
    ui['fork-dialog'].showModal();
  }

  ui['open-fork'].addEventListener('click', openForkDialog);
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
      if (login === loginGeneration) {
        if (Array.isArray(capabilities)) cachedCapabilities = capabilities;
        ui['capability-content'].textContent = recordText(capabilities);
        renderPluginsPage();
      }
    } catch (error) {
      if (login === loginGeneration) {
        ui['capability-content'].textContent = errorText(error);
        if (error.auth) disconnect(error.message);
      }
    } finally { ui['refresh-capabilities'].disabled = false; }
  });

  if (ui['nav-conversation']) ui['nav-conversation'].addEventListener('click', () => switchPage('conversation'));
  if (ui['nav-projects']) ui['nav-projects'].addEventListener('click', () => switchPage('projects'));
  if (ui['nav-plugins']) ui['nav-plugins'].addEventListener('click', () => switchPage('plugins'));
  if (ui['refresh-projects']) ui['refresh-projects'].addEventListener('click', refreshProjects);
  if (ui['refresh-plugins']) ui['refresh-plugins'].addEventListener('click', refreshPlugins);

  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && token && selected) schedulePoll(0);
  });
  window.addEventListener('online', () => { if (token && selected) schedulePoll(0); });
  window.addEventListener('pagehide', () => { token = ''; for (const controller of controllers) controller.abort(); });
}
