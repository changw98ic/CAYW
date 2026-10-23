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
const readMethods = new Set(['runtime.health', 'devices.list', 'device.binding.status', 'server.auth.status', 'projects.list', 'projects.browse', 'skills.catalog', 'mcp.catalog', 'session.list', 'session.get', 'session.events', 'session.grants', 'session.audit', 'capabilities.list']);
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
      workspace: clip(project.workspace || '', 240),
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

export {
  CORE_PROTOCOL_VERSION,
  MAX_MESSAGES,
  MAX_EVENTS,
  MAX_TEXT,
  MAX_RECORD,
  clip,
  object,
  safeCursor,
  array,
  recordText,
  readMethods,
  stateName,
  unresolved,
  frozen,
  canFork,
  stateLabel,
};
