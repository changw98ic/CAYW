// Run with: node --test crates/cayw-webui/tests/workbench.test.mjs
// Tests the WebUI workbench helpers: service URL generation and plan feedback validation.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const source = await readFile(new URL('../assets/app.js', import.meta.url), 'utf8');
const {
  buildVisualEditorLaunchUrl,
  buildTraceDebugLaunchUrl,
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
