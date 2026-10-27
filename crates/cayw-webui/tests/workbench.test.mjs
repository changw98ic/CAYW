// Run with: node --test crates/cayw-webui/tests/workbench.test.mjs
// Tests the WebUI workbench helpers: service URL generation, telemetry formatting,
// fleet formatting, and plan feedback validation.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const source = await readFile(new URL('../assets/app.js', import.meta.url), 'utf8');
const {
  buildVisualEditorLaunchUrl,
  buildTraceDebugLaunchUrl,
  formatContextOverview,
  formatRunInspectorSteps,
  formatFleetMembers,
  validatePlanFeedback,
} = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

for (const build of [buildVisualEditorLaunchUrl, buildTraceDebugLaunchUrl]) {
  test(`${build.name} opens the actual local service address without credentials or invented selectors`, () => {
    assert.equal(build({ baseUrl: 'http://127.0.0.1:5173/' }), 'http://127.0.0.1:5173/');
    assert.equal(build({ baseUrl: 'http://localhost:4200/' }), 'http://localhost:4200/');
    assert.equal(build({ baseUrl: 'http://[::1]:4200/' }), 'http://[::1]:4200/');
    for (const baseUrl of [undefined, '', 'javascript:alert(1)', 'https://example.com/',
      'http://127.0.0.1:4200/?token=secret', 'http://user:secret@localhost:4200/', 'http://localhost:4200/#token']) {
      assert.throws(() => build({ baseUrl }));
    }
  });
}

test('formatContextOverview handles missing or full stats correctly', () => {
  const empty = formatContextOverview(null);
  assert.equal(empty.tokensText, '无用量数据');
  assert.equal(empty.hitRateText, '0.0%');
  assert.equal(empty.budgetUnknown, false);
  assert.equal(empty.ledgerSettled, 0);

  const full = formatContextOverview({
    tokens: {
      total_tokens: 1500,
      input: 1000,
      output: 300,
      cache_read: 150,
      cache_write: 50,
      reasoning: 0,
    },
    cache: {
      cache_read: 150,
      cache_write: 50,
      hit_rate: 0.1304,
    },
    cost: {
      status: 'reported',
      amount: '0.0042',
      currency: 'USD',
    },
    ledger_settled_tokens: 1450,
    ledger_reserved_tokens: 50,
    ledger_remaining_tokens: 6500,
    budget_unknown: false,
    request_count: 3,
    turn_count: 2,
    composition: [
      { kind: 'input', tokens: 1000 },
      { kind: 'output', tokens: 300 },
    ],
    evolution: [
      {
        request_id: 'req-turn-1',
        session_id: 's1',
        task_id: 'task-1',
        turn: 1,
        provider: 'test-provider',
        model: 'test-model',
        usage_status: 'reported',
        usage: {
          input: 800,
          output: 400,
          cache_read: 0,
          cache_write: 0,
          reasoning: 0,
          total_tokens: 1200,
        },
      },
      {
        request_id: 'req-turn-2',
        session_id: 's1',
        task_id: 'task-1',
        turn: 2,
        provider: 'test-provider',
        model: 'test-model',
        usage_status: 'reported',
        usage: {
          input: 1200,
          output: 600,
          cache_read: 0,
          cache_write: 0,
          reasoning: 0,
          total_tokens: 1800,
        },
      },
    ],
  });

  assert.match(full.tokensText, /总计: 1500/);
  assert.match(full.tokensText, /输入: 1000/);
  assert.equal(full.hitRateText, '13.0%');
  assert.equal(full.costText, 'reported (0.0042 USD)');
  assert.equal(full.ledgerSettled, 1450);
  assert.equal(full.ledgerReserved, 50);
  assert.equal(full.ledgerRemaining, 6500);
  assert.equal(full.budgetUnknown, false);
  assert.equal(full.composition.length, 2);
  assert.equal(full.evolution.length, 2);
});

test('formatRunInspectorSteps maps raw runtime steps into clean UI records', () => {
  const steps = formatRunInspectorSteps([
    {
      step_id: 'step-1',
      turn: 1,
      ordinal: 0,
      tool: 'fs.read',
      outcome: 'success',
      elapsed_ms: 12,
      started_at_ms: 1000,
    },
    {
      step_id: 'step-2',
      turn: 1,
      ordinal: 1,
      tool: 'fs.write',
      outcome: 'failed',
      elapsed_ms: 25,
      started_at_ms: 1015,
      error_summary: 'Permission denied',
    },
  ]);

  assert.equal(steps.length, 2);
  assert.equal(steps[0].stepId, 'step-1');
  assert.equal(steps[0].tool, 'fs.read');
  assert.equal(steps[0].outcome, 'success');
  assert.equal(steps[0].elapsedMs, 12);
  assert.equal(steps[0].errorSummary, null);

  assert.equal(steps[1].stepId, 'step-2');
  assert.equal(steps[1].outcome, 'failed');
  assert.equal(steps[1].errorSummary, 'Permission denied');
});

test('formatFleetMembers formats sub-agent fleet membership records', () => {
  const members = formatFleetMembers([
    {
      child_task_id: 'task-child-1',
      child_session_id: 'sess-child-1',
      parent_task_id: 'task-root-1',
      label: 'test-agent',
      depth: 1,
      mode: 'foreground',
      detached: false,
      state: 'running',
    },
    {
      child_task_id: 'task-child-2',
      child_session_id: 'sess-child-2',
      parent_task_id: 'task-root-1',
      label: 'background-worker',
      depth: 1,
      mode: 'background',
      detached: true,
      state: 'completed',
    },
  ]);

  assert.equal(members.length, 2);
  assert.equal(members[0].childTaskId, 'task-child-1');
  assert.equal(members[0].detached, false);
  assert.equal(members[0].state, 'running');

  assert.equal(members[1].childTaskId, 'task-child-2');
  assert.equal(members[1].detached, true);
  assert.equal(members[1].state, 'completed');
});

test('validatePlanFeedback enforces binding to authoritative plan facts', () => {
  // Plan not present: must fail
  assert.throws(
    () => validatePlanFeedback(null, { session_id: 's1', decision: 'approve' }),
    /当前会话尚未生成计划，无法提交批注/
  );
  assert.throws(
    () => validatePlanFeedback({}, { session_id: 's1', decision: 'approve' }),
    /当前会话尚未生成计划，无法提交批注/
  );

  // Missing decision: must fail
  const validPlan = { task_id: 'task-root-1', version: 2, content_sha256: 'a'.repeat(64), steps: [{ id: '1', text: 'Review', status: 'pending' }] };
  assert.throws(
    () => validatePlanFeedback(validPlan, { session_id: 's1' }),
    /必须选择批注决定/
  );

  // Valid feedback bound to plan
  const validated = validatePlanFeedback(validPlan, {
    session_id: 's1',
    decision: 'approve_with_notes',
    summary: '通过但需修改',
    comments: [{ comment_id: 'c1', body: '请补充单元测试' }],
  });

  assert.equal(validated.session_id, 's1');
  assert.equal(validated.expected_task_id, validPlan.task_id);
  assert.equal(validated.expected_version, validPlan.version);
  assert.equal(validated.expected_content_sha256, validPlan.content_sha256);
  assert.throws(() => validatePlanFeedback({ ...validPlan, content_sha256: '' }, { decision: 'approve' }), /版本和内容摘要/);
  assert.equal(validated.decision, 'approve_with_notes');
  assert.equal(validated.summary, '通过但需修改');
  assert.equal(validated.comments.length, 1);
  assert.equal(validated.comments[0].body, '请补充单元测试');
  // Ensures arbitrary user-injected file_version / fake target fields are excluded
  assert.equal(validated.file_version, undefined);
  assert.equal(validated.target, undefined);
});
