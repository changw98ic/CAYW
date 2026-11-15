//! Offline integration tests for context compression after CodeMode declarations,
//! asynchronous background semantic summary and compression, post-turn automatic
//! memory extraction, proactive execution, forked cursor provenance, and dispatch
//! association recovery.
//!
//! Every test runs in a temporary directory with offline synthetic providers.
//! No test reads a user directory, calls a real model, or touches the network.

use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cayw_ai::{
    AssistantEvent, AssistantMessage, AssistantStream, CancellationToken, Content, Message, Model,
    ModelRequest, Provider, StopReason, Usage, UsageStatus, current_system,
};
use cayw_ipc::RpcHandler;
use cayw_runtime::{
    AcceptanceCheck, AcceptanceCriterion, AcceptanceResult, AcceptanceSource, FileRules,
    MemoryCandidate, MemoryOperation, ProactiveMode, ProactiveTriggerOutcome, Runtime,
    RuntimeConfig, SourceType, TaskDispatch, TaskInput, TaskIntent, TaskPriority, TaskResources,
    TaskState, WorkControl, WorkDispatchReply, WorkDispatchRequest, WorkItemSpec, WorkOwner,
    WorkRouter, WorkRouterError, WorkStatusReport, WorkTargetOutcome, WorkTaskQuery,
    fixture::script_response,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Notify;

fn assistant_text(model: &Model, text: impl Into<String>) -> AssistantMessage {
    let mut message = AssistantMessage::new(model);
    message.content.push(Content::text(text));
    message.stop_reason = StopReason::Stop;
    message.usage = Usage {
        input: 8,
        output: 8,
        total_tokens: 16,
        ..Default::default()
    };
    message.usage_status = UsageStatus::Reported;
    message
}

fn system_prompt_of(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|msg| match msg {
            Message::System(sys) => Some(sys.content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn codemode_mid_transcript_declaration_and_background_semantic_maintenance_preserve_final_decisions_and_replace_history()
 {
    #[derive(Default)]
    struct SharedState {
        gate_maintenance: AtomicBool,
        maintenance_entered: Notify,
        release_maintenance: Notify,
        summary_inputs: Mutex<Vec<String>>,
        compression_inputs: Mutex<Vec<String>>,
        foreground_requests: Mutex<Vec<Vec<Message>>>,
        turn_counter: AtomicUsize,
    }

    struct ArcMaintenanceProvider(Arc<SharedState>);

    impl Provider for ArcMaintenanceProvider {
        fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
            let state = self.0.clone();
            let sys = system_prompt_of(&request);
            let last_text = request
                .messages
                .last()
                .map(Message::text)
                .unwrap_or_default();

            if sys.contains("Summarize this completed conversation segment") {
                state.summary_inputs.lock().unwrap().push(last_text.clone());
                return Box::pin(futures::stream::once(async move {
                    state.maintenance_entered.notify_one();
                    while state.gate_maintenance.load(Ordering::Acquire) {
                        tokio::select! {
                            () = cancel.cancelled() => {
                                return AssistantEvent::Error {
                                    message: AssistantMessage::new(&request.model)
                                        .failure(StopReason::Aborted, "cancelled"),
                                };
                            }
                            () = state.release_maintenance.notified() => {}
                            () = tokio::time::sleep(Duration::from_millis(10)) => {}
                        }
                    }
                    let summary = if last_text.contains("最终决定改用方案B并保留全部历史日志")
                    {
                        "语义摘要：已执行 CodeMode 工具校验；初版假设方案A已被推翻，最终决定改用方案B并保留全部历史日志。"
                    } else {
                        "语义摘要：完成后续大容量校验轮次，维持方案B决定不变。"
                    };
                    AssistantEvent::Done {
                        message: assistant_text(&request.model, summary),
                    }
                }));
            }

            if sys.contains("Compress this historical session prefix") {
                state
                    .compression_inputs
                    .lock()
                    .unwrap()
                    .push(last_text.clone());
                return Box::pin(futures::stream::once(async move {
                    state.maintenance_entered.notify_one();
                    while state.gate_maintenance.load(Ordering::Acquire) {
                        tokio::select! {
                            () = cancel.cancelled() => {
                                return AssistantEvent::Error {
                                    message: AssistantMessage::new(&request.model)
                                        .failure(StopReason::Aborted, "cancelled"),
                                };
                            }
                            () = state.release_maintenance.notified() => {}
                            () = tokio::time::sleep(Duration::from_millis(10)) => {}
                        }
                    }
                    let compressed = "背景压缩归纳：会话已启用 CodeMode 执行并完成前序大批次负载验证；早期方案A已废弃，最终决定改用方案B并保留全部历史日志，未完成项为执行第三步收尾。";
                    AssistantEvent::Done {
                        message: assistant_text(&request.model, compressed),
                    }
                }));
            }

            state
                .foreground_requests
                .lock()
                .unwrap()
                .push(request.messages.clone());

            // Turn 1 uses real CodeMode `execute_code` so Core records a mid-transcript
            // System tool declaration before the tool exchange!
            let last_non_system = request
                .messages
                .iter()
                .rev()
                .find(|m| !matches!(m, Message::System(_)));
            let last_non_system_text = last_non_system.map(Message::text).unwrap_or_default();

            if matches!(last_non_system, Some(Message::User { .. }))
                && last_non_system_text.starts_with("第一轮：请运行工具检查负载")
            {
                let mut msg = script_response(
                    &request,
                    "cayw.emit('HEAVY_TOOL_PAYLOAD_' .. string.rep('X', 850))",
                    "cm-turn-1",
                );
                msg.stop_reason = StopReason::ToolUse;
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: msg,
                }]));
            }
            if let Some(Message::ToolResult(result)) = last_non_system {
                let needs_wait = result
                    .structured_content
                    .as_ref()
                    .and_then(|v| serde_json::from_value::<cayw_cells::CellPage>(v.clone()).ok())
                    .is_some_and(|p| p.vm_alive || !p.status.is_terminal() || p.has_more);
                if needs_wait {
                    let msg = script_response(&request, "", "cm-turn-1-wait");
                    return Box::pin(futures::stream::iter([AssistantEvent::Done {
                        message: msg,
                    }]));
                }
                let n = state.turn_counter.fetch_add(1, Ordering::AcqRel);
                let reply = format!(
                    "已读取负载（批次 {n}）。{}。初版采用方案A；但经核对尾部约束，最终决定改用方案B并保留全部历史日志。",
                    "填充说明".repeat(40)
                );
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_text(&request.model, reply),
                }]));
            }

            let reply = if last_non_system_text.starts_with("第二轮：补充大批次背景") {
                format!("第二轮回复：{}。继续沿用方案B。", "细节记录".repeat(60))
            } else {
                "第三轮完成。".to_owned()
            };
            Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: assistant_text(&request.model, reply),
            }]))
        }
    }

    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let state = Arc::new(SharedState::default());
    state.gate_maintenance.store(true, Ordering::Release);

    let mut model = Model::new("fixture", "exec-context");
    // input_limit = 3400 - 256 - 1024 = 2120; replace_at = 1590; compress_at = 795.
    model.context_window = Some(3_400);
    model.max_output_tokens = Some(256);

    let runtime = Runtime::open(
        temp.path().join("sessions.sqlite"),
        &workspace,
        Arc::new(ArcMaintenanceProvider(state.clone())),
        model,
        RuntimeConfig::default(),
    )
    .unwrap();
    let session = runtime.create(Some("CodeMode Context".into())).unwrap().id;

    // Foreground Turn 1 MUST return even while background maintenance is held on `gate_maintenance`!
    tokio::time::timeout(
        Duration::from_secs(5),
        runtime.prompt(
            &session,
            "第一轮：请运行工具检查负载，先按方案A试探，但以最后核对结论为准",
            CancellationToken::new(),
        ),
    )
    .await
    .expect("foreground prompt must never block on background summary or compression")
    .unwrap();

    // Release background maintenance and wait for summary + compression to publish.
    state.gate_maintenance.store(false, Ordering::Release);
    state.release_maintenance.notify_waiters();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let summaries = runtime.context_summaries(&session).unwrap();
        let has_semantic_summary = summaries
            .iter()
            .any(|s| !s.local_reuse && s.text.contains("最终决定改用方案B并保留全部历史日志"));
        let has_compression_call = !state.compression_inputs.lock().unwrap().is_empty();
        if has_semantic_summary && has_compression_call {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for background summary and compression; summaries={summaries:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Verify background summary & compression received the FULL tail decision, not just a truncated prefix.
    {
        let sum_inputs = state.summary_inputs.lock().unwrap();
        assert!(
            sum_inputs
                .iter()
                .any(|input| input.contains("最终决定改用方案B并保留全部历史日志")),
            "summary maintenance input must include the end-of-turn decision: {sum_inputs:?}"
        );
    }

    // Turn 2 pushes total raw session history well past `replace_at` (1,590 bytes).
    let turn2_prompt = format!("第二轮：补充大批次背景 {}", "上下文扩展".repeat(45));
    runtime
        .prompt(&session, &turn2_prompt, CancellationToken::new())
        .await
        .unwrap();

    // Wait briefly so any Turn 2 background jobs settle cleanly before Turn 3.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let summaries = runtime.context_summaries(&session).unwrap();
        if summaries.iter().filter(|s| !s.local_reuse).count() >= 3 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Turn 3 exceeds `replace_at`: foreground preparation locally assembles compressed history
    // AFTER the mid-transcript CodeMode `execute_code` declaration while keeping `execute_code` intact!
    runtime
        .prompt(&session, "第三轮：执行收尾", CancellationToken::new())
        .await
        .unwrap();

    let last_foreground = state
        .foreground_requests
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("captured Turn 3 foreground request");

    let declared_tools = current_system(&last_foreground).tools;
    assert!(
        declared_tools.iter().any(|t| t.name == "execute_code"),
        "mid-transcript CodeMode tool declaration must stay active after history replacement: {declared_tools:?}"
    );
    assert!(
        last_foreground
            .iter()
            .any(|m| m.text().contains("最终决定改用方案B并保留全部历史日志")),
        "assembled provider input must carry the synthesized final decision: {last_foreground:?}"
    );
    assert!(
        !last_foreground.iter().any(|m| m
            .text()
            .contains(&format!("HEAVY_TOOL_PAYLOAD_{}", "X".repeat(850)))),
        "compressible post-declaration tool payload must be replaced instead of pinned raw"
    );

    // Durable Core history remains completely intact!
    let view = runtime.get(&session).unwrap();
    assert!(
        view.messages
            .iter()
            .any(|m| m.text().contains("HEAVY_TOOL_PAYLOAD_")),
        "durable session transcript must never be deleted by context assembly"
    );

    runtime.shutdown_background();
}

#[tokio::test]
async fn post_turn_memory_writer_skips_confirmations_extracts_verified_preferences_and_protects_user_set()
 {
    struct MemoryWriterProvider {
        writer_calls: AtomicUsize,
        protected_entry_id: Mutex<Option<String>>,
    }

    impl Provider for MemoryWriterProvider {
        fn stream(&self, request: ModelRequest, _cancel: CancellationToken) -> AssistantStream {
            let sys = system_prompt_of(&request);
            if sys.contains("You are the main-session memory writer") {
                self.writer_calls.fetch_add(1, Ordering::AcqRel);
                let prompt = request
                    .messages
                    .last()
                    .map(Message::text)
                    .unwrap_or_default();
                // Extract the `[session:cursor]` log_id from `New user-input turns:\n[<log_id>] ...`
                let log_id = prompt
                    .lines()
                    .find_map(|line| {
                        let rest = line.strip_prefix('[')?;
                        let (id, _) = rest.split_once(']')?;
                        Some(id.to_owned())
                    })
                    .expect("writer prompt lists user turn log_id");
                let protected_id = self.protected_entry_id.lock().unwrap().clone();
                let mut candidates = vec![json!({
                    "operation": "upsert",
                    "entry_id": null,
                    "kind": "preference",
                    "body": "汇报统一使用短句并附带实际跑过的测试结果",
                    "source_type": "stated",
                    "source_log_ids": [log_id],
                })];
                if let Some(uid) = protected_id {
                    candidates.push(json!({
                        "operation": "upsert",
                        "entry_id": uid,
                        "kind": "base",
                        "body": "后台试图覆盖用户手写记忆",
                        "source_type": "stated",
                        "source_log_ids": [log_id],
                    }));
                }
                let payload = json!({
                    "candidates": candidates,
                    "forget": []
                })
                .to_string();
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_text(&request.model, payload),
                }]));
            }

            Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: assistant_text(&request.model, "已收到。"),
            }]))
        }
    }

    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("main-workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let provider = Arc::new(MemoryWriterProvider {
        writer_calls: AtomicUsize::new(0),
        protected_entry_id: Mutex::new(None),
    });
    let mut model = Model::new("fixture", "main-mem");
    model.context_window = Some(32_768);
    model.max_output_tokens = Some(512);

    let runtime = Runtime::open(
        temp.path().join("main.sqlite"),
        &workspace,
        provider.clone(),
        model,
        RuntimeConfig {
            main_session: true,
            approval_rules: Some(Arc::new(FileRules::new(temp.path().join("rules.json")))),
            ..Default::default()
        },
    )
    .unwrap();
    let session = runtime.create(Some("Main".into())).unwrap().id;

    // Turn 1: pure confirmation ("好的") -> advances frontier with ZERO background writer model calls!
    runtime
        .prompt(&session, "好的", CancellationToken::new())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while runtime.memory().writer_frontier(&session).unwrap() == 0
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(
        runtime.memory().writer_frontier(&session).unwrap() > 0,
        "pure confirmation turn should advance writer_frontier locally"
    );
    assert_eq!(
        provider.writer_calls.load(Ordering::Acquire),
        0,
        "pure confirmation must not invoke the background memory writer model"
    );

    // Seed a `user_set` memory entry by creating one from verified evidence and editing it on disk.
    let turn1_cursor = runtime.memory().writer_frontier(&session).unwrap();
    let seed_report = runtime
        .apply_memory_candidates(
            &session,
            &[MemoryCandidate {
                operation: MemoryOperation::Upsert,
                entry_id: None,
                kind: "base".into(),
                body: "初始项目名：CAYW".into(),
                source_type: SourceType::Stated,
                source_log_ids: vec![format!("{session}:{turn1_cursor}")],
            }],
        )
        .unwrap();
    let protected_id = seed_report.written[0].clone();
    let entry_path = workspace.join("memory").join(format!("{protected_id}.md"));
    let raw = std::fs::read_to_string(&entry_path).unwrap();
    let (header, _) = raw.split_once("\n\n").unwrap();
    std::fs::write(&entry_path, format!("{header}\n\n用户手动锁定的核心约束")).unwrap();
    runtime.memory().scan().unwrap();
    *provider.protected_entry_id.lock().unwrap() = Some(protected_id.clone());

    // Turn 2: real user preference statement -> post-turn automatic memory writer extracts preference
    // and records the `user_set` overwrite attempt as a pending candidate!
    runtime
        .prompt(
            &session,
            "请记住：以后所有汇报统一使用短句并附带实际跑过的测试结果",
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let entries = runtime.memory_entries().unwrap();
        let pending = runtime.pending_memory_candidates().unwrap();
        let has_auto_pref = entries.iter().any(|e| {
            e.kind == "preference"
                && e.source_type == SourceType::Stated
                && e.body.contains("汇报统一使用短句")
        });
        let has_pending_conflict = pending
            .iter()
            .any(|p| p.candidate.entry_id.as_deref() == Some(protected_id.as_str()));
        if has_auto_pref && has_pending_conflict {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for automatic post-turn memory writer; entries={entries:?}, pending={pending:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(runtime.memory().writer_frontier(&session).unwrap() > turn1_cursor);
    assert_eq!(provider.writer_calls.load(Ordering::Acquire), 1);
    let entries = runtime.memory_entries().unwrap();
    let user_locked = entries
        .iter()
        .find(|e| e.entry_id == protected_id)
        .expect("user_set entry remains visible");
    assert_eq!(user_locked.source_type, SourceType::UserSet);
    assert_eq!(user_locked.body, "用户手动锁定的核心约束");

    runtime.shutdown_background();
}

#[tokio::test]
async fn proactive_auto_trigger_runs_on_dedicated_session_when_main_is_busy_and_recovers_duplicate_triggers()
 {
    struct ArcGatedProvider {
        main_entered: Arc<Notify>,
        release_main: Arc<Notify>,
        hold_main: Arc<AtomicBool>,
    }

    impl Provider for ArcGatedProvider {
        fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
            let last_non_system_text = request
                .messages
                .iter()
                .rev()
                .find(|m| !matches!(m, Message::System(_)))
                .map(Message::text)
                .unwrap_or_default();
            let entered = self.main_entered.clone();
            let release = self.release_main.clone();
            let hold = self.hold_main.clone();
            if last_non_system_text == "HOLD_MAIN_SESSION" {
                return Box::pin(futures::stream::once(async move {
                    entered.notify_one();
                    while hold.load(Ordering::Acquire) {
                        tokio::select! {
                            () = cancel.cancelled() => {
                                return AssistantEvent::Error {
                                    message: AssistantMessage::new(&request.model)
                                        .failure(StopReason::Aborted, "cancelled"),
                                };
                            }
                            () = release.notified() => {}
                            () = tokio::time::sleep(Duration::from_millis(10)) => {}
                        }
                    }
                    AssistantEvent::Done {
                        message: assistant_text(&request.model, "主会话前台完成"),
                    }
                }));
            }

            Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: assistant_text(&request.model, "主动任务自动执行完成"),
            }]))
        }
    }

    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("main-proactive");
    std::fs::create_dir_all(&workspace).unwrap();

    let main_entered = Arc::new(Notify::new());
    let release_main = Arc::new(Notify::new());
    let hold_main = Arc::new(AtomicBool::new(true));

    let runtime = Runtime::open(
        temp.path().join("proactive.sqlite"),
        &workspace,
        Arc::new(ArcGatedProvider {
            main_entered: main_entered.clone(),
            release_main: release_main.clone(),
            hold_main: hold_main.clone(),
        }),
        Model::new("fixture", "proactive"),
        RuntimeConfig {
            main_session: true,
            max_concurrent_runs: 4,
            approval_rules: Some(Arc::new(FileRules::new(temp.path().join("rules.json")))),
            ..Default::default()
        },
    )
    .unwrap();

    let main_session = runtime.create(Some("Main".into())).unwrap().id;
    let work1 = runtime
        .create_work_item(WorkItemSpec {
            work_id: Some("work-busy-main".into()),
            owner_main_session_id: main_session.clone(),
            title: "巡检构建状态".into(),
            goal: "在主会话繁忙时仍能自动启动本地主动任务".into(),
            acceptance_criteria: vec![AcceptanceCriterion {
                criterion_id: "c1".into(),
                description: "check".into(),
                check: Some(AcceptanceCheck::PathExists {
                    path: "done.flag".into(),
                }),
            }],
            user_source_refs: vec![format!("{main_session}:1")],
            action_scope: vec!["workspace".into()],
            approval_refs: vec![],
            session_refs: vec![],
            task_refs: vec![],
            artifact_refs: vec![],
            waiting_conditions: vec![],
            pending_decisions: vec![],
            next_check_at: None,
        })
        .unwrap();

    // Hold `main_session` busy in a foreground prompt.
    let rt_for_main = runtime.clone();
    let main_sid = main_session.clone();
    let main_handle = tokio::spawn(async move {
        rt_for_main
            .prompt(&main_sid, "HOLD_MAIN_SESSION", CancellationToken::new())
            .await
    });
    main_entered.notified().await;

    // Trigger proactive Auto while main_session is busy -> creates dedicated session and runs task!
    let outcome = runtime
        .trigger_proactive(ProactiveMode::Auto, "rule-build", 1, &work1.work_id, 0)
        .unwrap();
    assert!(matches!(
        outcome,
        ProactiveTriggerOutcome::Dispatched { .. }
    ));
    let started = runtime
        .run_proactive_dispatch(&outcome)
        .await
        .unwrap()
        .expect("proactive task started");
    assert_ne!(
        started.session_id, main_session,
        "busy main session must not block proactive Auto task; a dedicated local session is used"
    );

    runtime.wait_idle(&started.session_id).await;
    let completed = runtime.task(&started.id).unwrap();
    assert_eq!(completed.state, TaskState::Completed);

    let updated_work1 = runtime.work_item(&work1.work_id).unwrap().unwrap();
    assert!(
        updated_work1
            .session_refs
            .iter()
            .any(|s| s.session_id == started.session_id)
    );
    assert!(
        updated_work1
            .task_refs
            .iter()
            .any(|t| t.task_id == started.id)
    );

    // Release the foreground prompt on `main_session`.
    hold_main.store(false, Ordering::Release);
    release_main.notify_waiters();
    main_handle.await.unwrap().unwrap();

    // Also verify Duplicate trigger recovery: if `trigger_proactive` enqueued a task without starting it
    // (e.g., interrupted before `run_proactive_dispatch`), a second `trigger_proactive` returns `Duplicate`
    // and `run_proactive_dispatch(&dup)` starts and completes that exact queued task without duplicating!
    let work2 = runtime
        .create_work_item(WorkItemSpec {
            work_id: Some("work-dup-recover".into()),
            owner_main_session_id: main_session.clone(),
            title: "恢复未启动的主动任务".into(),
            goal: "重复触发不重入队且能启动排队中的任务".into(),
            acceptance_criteria: vec![AcceptanceCriterion {
                criterion_id: "c1".into(),
                description: "check".into(),
                check: Some(AcceptanceCheck::PathExists {
                    path: "done2.flag".into(),
                }),
            }],
            user_source_refs: vec![format!("{main_session}:1")],
            action_scope: vec!["workspace".into()],
            approval_refs: vec![],
            session_refs: vec![],
            task_refs: vec![],
            artifact_refs: vec![],
            waiting_conditions: vec![],
            pending_decisions: vec![],
            next_check_at: None,
        })
        .unwrap();

    let first_outcome = runtime
        .trigger_proactive(ProactiveMode::Auto, "rule-dup", 1, &work2.work_id, 0)
        .unwrap();
    assert!(matches!(
        first_outcome,
        ProactiveTriggerOutcome::Dispatched { .. }
    ));

    let dup_outcome = runtime
        .trigger_proactive(ProactiveMode::Auto, "rule-dup", 1, &work2.work_id, 0)
        .unwrap();
    assert!(matches!(
        dup_outcome,
        ProactiveTriggerOutcome::Duplicate { .. }
    ));

    let recovered_task = runtime
        .run_proactive_dispatch(&dup_outcome)
        .await
        .unwrap()
        .expect("duplicate outcome recovers and starts the queued proactive task");
    runtime.wait_idle(&recovered_task.session_id).await;
    assert_eq!(
        runtime.task(&recovered_task.id).unwrap().state,
        TaskState::Completed
    );
    let work2_tasks = runtime
        .tasks(&recovered_task.session_id)
        .unwrap()
        .into_iter()
        .filter(|t| t.work_id.as_deref() == Some("work-dup-recover"))
        .collect::<Vec<_>>();
    assert_eq!(
        work2_tasks.len(),
        1,
        "duplicate proactive trigger must never create a second task"
    );

    runtime.shutdown_background();
}

#[tokio::test]
async fn forked_session_preserves_shared_fork_cursor_without_shifting_onto_post_fork_log_ids() {
    struct SimpleReply;
    impl Provider for SimpleReply {
        fn stream(&self, request: ModelRequest, _cancel: CancellationToken) -> AssistantStream {
            Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: assistant_text(&request.model, "简短回复"),
            }]))
        }
    }

    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("fork-workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let runtime = Runtime::open(
        temp.path().join("fork.sqlite"),
        &workspace,
        Arc::new(SimpleReply),
        Model::new("fixture", "fork-check"),
        RuntimeConfig {
            main_session: true,
            approval_rules: Some(Arc::new(FileRules::new(temp.path().join("rules.json")))),
            ..Default::default()
        },
    )
    .unwrap();

    let origin = runtime.create(Some("Origin".into())).unwrap().id;
    runtime
        .prompt(&origin, "原会话第一轮输入", CancellationToken::new())
        .await
        .unwrap();
    runtime
        .prompt(&origin, "原会话第二轮输入", CancellationToken::new())
        .await
        .unwrap();

    let forked = runtime.fork(&origin, "Forked", true).unwrap();
    // Append two short post-fork turns so SessionContexts records local summaries for both
    // the inherited prefix and the first post-fork turn.
    runtime
        .prompt(
            &forked.id,
            "分叉后的第一条真实用户输入",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    runtime
        .prompt(&forked.id, "分叉后的第二条输入", CancellationToken::new())
        .await
        .unwrap();

    let summaries = runtime.context_summaries(&forked.id).unwrap();
    assert!(
        summaries.len() >= 2,
        "expected summaries for inherited fork span and post-fork turn, got: {summaries:?}"
    );
    // In Core, event 1 is SessionCreated and event 2 is SessionForked (carrying all inherited messages).
    let inherited = summaries
        .iter()
        .find(|s| s.start_cursor == 2)
        .expect("inherited messages stay anchored to SessionForked cursor 2");
    assert_eq!(
        inherited.end_cursor, 2,
        "inherited messages must merge on cursor 2 instead of shifting onto cursors 3, 4, ..."
    );
    assert_eq!(inherited.evidence, vec![format!("{}:2", forked.id)]);

    let post_fork = summaries
        .iter()
        .find(|s| s.start_cursor > 2)
        .expect("post-fork turn has its own summary above cursor 2");
    assert!(
        post_fork.text.contains("分叉后的第一条真实用户输入"),
        "post-fork summary must cover the real post-fork turn: {post_fork:?}"
    );

    // And memory provenance on the forked session accepts the real post-fork user turn cursor
    // while rejecting the non-user assistant cursor immediately after it!
    let post_fork_user_log_id = format!("{}:{}", forked.id, post_fork.start_cursor);
    let post_fork_assistant_log_id = format!("{}:{}", forked.id, post_fork.end_cursor);
    let rejected_report = runtime
        .apply_memory_candidates(
            &forked.id,
            &[MemoryCandidate {
                operation: MemoryOperation::Upsert,
                entry_id: None,
                kind: "preference".into(),
                body: "误指向模型游标的候选".into(),
                source_type: SourceType::Stated,
                source_log_ids: vec![post_fork_assistant_log_id],
            }],
        )
        .unwrap();
    assert!(rejected_report.written.is_empty());
    assert_eq!(rejected_report.rejected.len(), 1);

    let report = runtime
        .apply_memory_candidates(
            &forked.id,
            &[MemoryCandidate {
                operation: MemoryOperation::Upsert,
                entry_id: None,
                kind: "preference".into(),
                body: "分叉后新增的用户偏好".into(),
                source_type: SourceType::Stated,
                source_log_ids: vec![post_fork_user_log_id],
            }],
        )
        .unwrap();
    assert_eq!(report.written.len(), 1);
    assert!(report.rejected.is_empty());

    runtime.shutdown_background();
}

#[tokio::test]
async fn unattached_external_dispatch_recovers_via_work_tasks_and_work_dispatch_and_reports_ids_on_attach_failure()
 {
    struct Script(String);
    impl Provider for Script {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let reminded = request.messages.last().is_some_and(|message| {
                message
                    .text()
                    .contains("This main-session turn cannot finish yet")
            });
            let message = if reminded {
                assistant_text(&request.model, "stopped after the route reminder")
            } else {
                script_response(
                    &request,
                    &self.0,
                    &format!("recover-{}", request.messages.len()),
                )
            };
            Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
        }
    }

    struct TargetRouter {
        target: Runtime,
        workspace: String,
        bad_task_id_on_key: Option<String>,
    }

    #[async_trait]
    impl WorkRouter for TargetRouter {
        async fn workspaces(&self) -> Result<Vec<Value>, WorkRouterError> {
            Ok(vec![json!({
                "id": self.workspace,
                "path": self.target.workspace().to_string_lossy(),
                "state": "open"
            })])
        }

        async fn sessions(&self, workspace: &str) -> Result<Vec<Value>, WorkRouterError> {
            assert_eq!(workspace, self.workspace);
            self.target
                .list()
                .map_err(|e| WorkRouterError::known(e.to_string()))?
                .into_iter()
                .map(|s| serde_json::to_value(s).map_err(|e| WorkRouterError::known(e.to_string())))
                .collect()
        }

        async fn create_session(
            &self,
            workspace: &str,
            label: &str,
        ) -> Result<Value, WorkRouterError> {
            assert_eq!(workspace, self.workspace);
            let s = self
                .target
                .create(Some(label.to_owned()))
                .map_err(|e| WorkRouterError::known(e.to_string()))?;
            serde_json::to_value(s).map_err(|e| WorkRouterError::known(e.to_string()))
        }

        async fn dispatch(
            &self,
            request: WorkDispatchRequest,
        ) -> Result<WorkDispatchReply, WorkRouterError> {
            assert_eq!(request.workspace, self.workspace);
            let session_id = request.session_id.unwrap();
            if self
                .bad_task_id_on_key
                .as_deref()
                .is_some_and(|k| k == request.dispatch_key)
            {
                // Simulate a dispatched task whose subsequent `attach_task_ref` fails validation
                // so we can assert the actionable recovery identifiers in the error message.
                return Ok(WorkDispatchReply {
                    workspace: self.workspace.clone(),
                    session_id,
                    task_id: " ".into(),
                    state: TaskState::Running,
                    work_id: request.work_id,
                    dispatch_key: request.dispatch_key,
                    reused: false,
                });
            }
            let reused = self
                .target
                .task_by_dispatch_key(&request.dispatch_key)
                .map_err(|e| WorkRouterError::known(e.to_string()))?
                .is_some();
            let task = self
                .target
                .enqueue_dispatched(
                    &session_id,
                    TaskInput {
                        text: request.text,
                        images: Vec::new(),
                    },
                    TaskIntent::General,
                    request.priority,
                    TaskResources {
                        declared_memory_bytes: request.declared_memory_bytes,
                        ..Default::default()
                    },
                    TaskDispatch {
                        work_id: request.work_id.clone(),
                        dispatch_key: request.dispatch_key.clone(),
                        pause_version: request.pause_version,
                        stop_version: request.stop_version,
                    },
                )
                .map_err(|e| WorkRouterError::known(e.to_string()))?;
            Ok(WorkDispatchReply {
                workspace: self.workspace.clone(),
                session_id,
                task_id: task.id,
                state: task.state,
                work_id: request.work_id,
                dispatch_key: request.dispatch_key,
                reused,
            })
        }

        async fn task(
            &self,
            query: WorkTaskQuery,
        ) -> Result<Option<cayw_runtime::TaskRecord>, WorkRouterError> {
            assert_eq!(query.workspace, self.workspace);
            if let Some(key) = query.dispatch_key.as_deref() {
                return self
                    .target
                    .task_by_dispatch_key(key)
                    .map_err(|e| WorkRouterError::known(e.to_string()));
            }
            let id = query.task_id.unwrap();
            self.target
                .task(&id)
                .map(Some)
                .map_err(|e| WorkRouterError::known(e.to_string()))
        }

        async fn control(
            &self,
            action: WorkControl,
            work_id: &str,
            version: u64,
            workspaces: &[String],
        ) -> Result<Vec<WorkTargetOutcome>, WorkRouterError> {
            let mut out = Vec::new();
            for ws in workspaces {
                assert_eq!(ws, &self.workspace);
                let updated = match action {
                    WorkControl::Pause => self.target.apply_work_pause(work_id, version),
                    WorkControl::Stop => self.target.apply_work_stop(work_id, version),
                }
                .map_err(|e| WorkRouterError::known(e.to_string()))?;
                out.push(WorkTargetOutcome {
                    workspace: ws.clone(),
                    updated: Some(updated),
                    error: None,
                });
            }
            Ok(out)
        }

        async fn evaluate_check(
            &self,
            _workspace: &str,
            _check: &AcceptanceCheck,
        ) -> Result<bool, WorkRouterError> {
            Ok(false)
        }

        async fn notify_session(
            &self,
            notice: cayw_runtime::WorkSessionNotice,
        ) -> Result<cayw_runtime::WorkSessionNoticeReply, WorkRouterError> {
            self.target
                .deliver_session_notice(&notice)
                .map_err(|error| WorkRouterError::known(error.to_string()))
        }
    }

    fn open_rt(dir: &Path, provider: Arc<dyn Provider>, config: RuntimeConfig) -> Runtime {
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        Runtime::open(
            dir.join("db.sqlite"),
            dir.join("ws"),
            provider,
            Model::new("fixture", "recover"),
            config,
        )
        .unwrap()
    }

    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let target = open_rt(
        &root.join("target"),
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        RuntimeConfig::default(),
    );
    let target_session = target.create(Some("Worker 1".into())).unwrap().id;
    let target_session_2 = target.create(Some("Worker 2".into())).unwrap().id;

    let script = format!(
        r#"
cayw.call('work.list', {{}})
local w1 = cayw.call('work.create', {{
  work_id='work-orphan-1',
  title='recover via work.tasks',
  goal='recover unattached dispatch without duplicating',
  acceptance_criteria={{ {{criterion_id='c1', description='check'}} }},
  session_refs={{ {{session_id='{target_session}', workspace='target'}} }}
}})
local w2 = cayw.call('work.create', {{
  work_id='work-orphan-paused',
  title='recover via work.dispatch while paused',
  goal='re-associate paused work dispatch and sync pause_version',
  acceptance_criteria={{ {{criterion_id='c2', description='check'}} }},
  session_refs={{ {{session_id='{target_session_2}', workspace='target'}} }}
}})
cayw.call('work.pause', {{work_id='work-orphan-paused'}})
return {{w1=w1.work_id, w2=w2.work_id}}
"#
    );

    let main_step1 = open_rt(
        &root.join("main"),
        Arc::new(Script(script)),
        RuntimeConfig {
            main_session: true,
            approval_rules: Some(Arc::new(FileRules::new(root.join("rules.json")))),
            ..Default::default()
        },
    );
    main_step1
        .set_work_router(Arc::new(TargetRouter {
            target: target.clone(),
            workspace: "target".into(),
            bad_task_id_on_key: Some("dk-attach-fail".into()),
        }))
        .unwrap();
    let main_session = main_step1.create(Some("Main".into())).unwrap().id;
    let refused = main_step1
        .prompt(&main_session, "create items", CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("check existing WorkItems"),
        "{refused}"
    );
    let created = main_step1.list_work_items().unwrap();
    assert_eq!(created.len(), 2);
    assert!(
        created.iter().all(|item| item.task_refs.is_empty()),
        "creating without dispatch must not attach the later orphan tasks"
    );
    drop(main_step1);

    // Pre-seed two unattached dispatched tasks directly in `target` (simulating failure right before `attach_task_ref`).
    let orphan1 = target
        .enqueue_dispatched(
            &target_session,
            TaskInput {
                text: "orphan 1".into(),
                images: Vec::new(),
            },
            TaskIntent::General,
            TaskPriority::Normal,
            TaskResources::default(),
            TaskDispatch {
                work_id: "work-orphan-1".into(),
                dispatch_key: "dk-orphan-1".into(),
                pause_version: 0,
                stop_version: 0,
            },
        )
        .unwrap();
    let orphan2 = target
        .enqueue_dispatched(
            &target_session_2,
            TaskInput {
                text: "orphan 2".into(),
                images: Vec::new(),
            },
            TaskIntent::General,
            TaskPriority::Normal,
            TaskResources::default(),
            TaskDispatch {
                work_id: "work-orphan-paused".into(),
                dispatch_key: "dk-orphan-paused".into(),
                pause_version: 0,
                stop_version: 0,
            },
        )
        .unwrap();

    let script_step2 = format!(
        r#"
cayw.call('work.list', {{}})
local recovered_tasks = cayw.call('work.tasks', {{
  work_id='work-orphan-1',
  workspace='target',
  dispatch_key='dk-orphan-1'
}})
local recovered_paused = cayw.call('work.dispatch', {{
  work_id='work-orphan-paused',
  workspace='target',
  session_id='{target_session_2}',
  text='orphan 2',
  dispatch_key='dk-orphan-paused'
}})
local ok, attach_err = pcall(function()
  return cayw.call('work.dispatch', {{
    work_id='work-orphan-1',
    workspace='target',
    session_id='{target_session}',
    text='trigger attach error',
    dispatch_key='dk-attach-fail'
  }})
end)
assert(not ok, 'attach failure must not pretend success')
local err_text = tostring(attach_err)
assert(string.find(err_text, 'target', 1, true) ~= nil, err_text)
assert(string.find(err_text, '{target_session}', 1, true) ~= nil, err_text)
assert(string.find(err_text, 'work-orphan-1', 1, true) ~= nil, err_text)
assert(string.find(err_text, 'dk-attach-fail', 1, true) ~= nil, err_text)
return {{
  tasks_count = #recovered_tasks,
  reused = recovered_paused.dispatch.reused,
  err = err_text
}}
"#
    );

    let main_step2 = open_rt(
        &root.join("main"),
        Arc::new(Script(script_step2)),
        RuntimeConfig {
            main_session: true,
            approval_rules: Some(Arc::new(FileRules::new(root.join("rules.json")))),
            ..Default::default()
        },
    );
    main_step2
        .set_work_router(Arc::new(TargetRouter {
            target: target.clone(),
            workspace: "target".into(),
            bad_task_id_on_key: Some("dk-attach-fail".into()),
        }))
        .unwrap();
    main_step2
        .prompt(&main_session, "recover orphans", CancellationToken::new())
        .await
        .unwrap();

    let item1 = main_step2.work_item("work-orphan-1").unwrap().unwrap();
    assert_eq!(item1.task_refs.len(), 1);
    assert_eq!(item1.task_refs[0].task_id, orphan1.id);
    assert_eq!(item1.task_refs[0].workspace.as_deref(), Some("target"));

    let item2 = main_step2.work_item("work-orphan-paused").unwrap().unwrap();
    assert_eq!(item2.task_refs.len(), 1);
    assert_eq!(item2.task_refs[0].task_id, orphan2.id);
    assert_eq!(
        target.task(&orphan2.id).unwrap().pause_version,
        1,
        "recovering an unattached dispatch on a paused WorkItem must also sync pause_version"
    );
    assert_eq!(
        target.tasks(&target_session).unwrap().len(),
        1,
        "recovery must attach existing dispatched task on session 1 without creating duplicates"
    );
    assert_eq!(
        target.tasks(&target_session_2).unwrap().len(),
        1,
        "recovery must attach existing dispatched task on session 2 without creating duplicates"
    );

    main_step2.shutdown_background();
    target.shutdown_background();
}

#[tokio::test]
async fn product_model_without_context_window_runs_background_maintenance_and_surfaces_writer_failures()
 {
    struct FallbackWindowProvider {
        fail_writer_once: AtomicBool,
        summary_calls: AtomicUsize,
        writer_calls: AtomicUsize,
    }

    impl Provider for FallbackWindowProvider {
        fn stream(&self, request: ModelRequest, _cancel: CancellationToken) -> AssistantStream {
            let sys = system_prompt_of(&request);
            if sys.contains("Summarize this completed conversation segment") {
                self.summary_calls.fetch_add(1, Ordering::AcqRel);
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_text(
                        &request.model,
                        "无 context_window 配置下按保守默认窗口生成的后台摘要",
                    ),
                }]));
            }
            if sys.contains("You are the main-session memory writer") {
                self.writer_calls.fetch_add(1, Ordering::AcqRel);
                if self.fail_writer_once.swap(false, Ordering::AcqRel) {
                    return Box::pin(futures::stream::iter([AssistantEvent::Done {
                        message: assistant_text(&request.model, "NOT_VALID_JSON"),
                    }]));
                }
                let prompt = request
                    .messages
                    .last()
                    .map(Message::text)
                    .unwrap_or_default();
                let log_id = prompt
                    .lines()
                    .find_map(|line| {
                        let rest = line.strip_prefix('[')?;
                        let (id, _) = rest.split_once(']')?;
                        Some(id.to_owned())
                    })
                    .expect("writer prompt lists user turn log_id");
                let payload = json!({
                    "candidates": [{
                        "operation": "upsert",
                        "entry_id": null,
                        "kind": "preference",
                        "body": "未配置 context_window 时仍能自动提取记忆",
                        "source_type": "stated",
                        "source_log_ids": [log_id],
                    }],
                    "forget": []
                })
                .to_string();
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_text(&request.model, payload),
                }]));
            }
            let reply = format!("前台长回复：{}", "展开细节说明".repeat(90));
            Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: assistant_text(&request.model, reply),
            }]))
        }
    }

    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("fallback-window-ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let model_cfg: cayw_runtime::ProviderConfig = serde_json::from_value(json!({
        "service": "openai",
        "api": "chat_completions",
        "id": "custom-large-output-model",
        "base_url": "http://127.0.0.1:9/v1",
        "api_version": "v1",
        "max_output_tokens": 40000
    }))
    .unwrap();
    assert!(!model_cfg.has_context_window());
    assert!(
        model_cfg
            .context_window_warning(true)
            .expect("missing context_window emits advisory")
            .contains("context_window")
    );

    let mut config = serde_json::from_value::<cayw_runtime::HeadlessConfig>(json!({}))
        .unwrap()
        .into_runtime_config()
        .unwrap();
    assert!(config.background_model_maintenance);
    config.main_session = true;
    config.approval_rules = Some(Arc::new(FileRules::new(temp.path().join("rules.json"))));

    let provider = Arc::new(FallbackWindowProvider {
        fail_writer_once: AtomicBool::new(true),
        summary_calls: AtomicUsize::new(0),
        writer_calls: AtomicUsize::new(0),
    });
    let mut model = Model::new("openai", "custom-large-output-model");
    model.context_window = None;
    model.max_output_tokens = Some(40_000);

    let runtime = Runtime::open(
        temp.path().join("fallback.sqlite"),
        &workspace,
        provider.clone(),
        model,
        config,
    )
    .unwrap();
    let session = runtime.create(Some("Main Fallback".into())).unwrap().id;

    // Long Turn 1 (> 384 bytes) with `context_window: None` and `max_output_tokens: 40_000`:
    // background summary succeeds, while the first memory_writer call returns invalid JSON and records failure!
    let long_input = format!(
        "请记住：未配置 context_window 时仍能自动提取记忆。{}",
        "补充上下文载荷".repeat(30)
    );
    runtime
        .prompt(&session, &long_input, CancellationToken::new())
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let summaries = runtime.context_summaries(&session).unwrap();
        let mem_status = runtime.memory().status().unwrap();
        let has_model_summary = summaries.iter().any(|s| !s.local_reuse);
        if has_model_summary && mem_status.writer_failures == 1 {
            assert!(
                mem_status
                    .last_writer_error
                    .as_deref()
                    .is_some_and(|err| err.contains("invalid memory writer JSON")),
                "unexpected writer error: {mem_status:?}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for fallback-window summary & writer failure; summaries={summaries:?}, mem_status={mem_status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        runtime.memory().writer_frontier(&session).unwrap(),
        0,
        "failed writer attempt must not advance writer_frontier"
    );

    // Turn 2 retries from cursor 0 and extracts the memory entry cleanly!
    runtime
        .prompt(
            &session,
            "再确认一次偏好已生效并继续工作",
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let entries = runtime.memory_entries().unwrap();
        if entries
            .iter()
            .any(|e| e.body.contains("未配置 context_window 时仍能自动提取记忆"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for recovered memory writer; entries={entries:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(provider.summary_calls.load(Ordering::Acquire) >= 1);
    assert!(provider.writer_calls.load(Ordering::Acquire) >= 2);

    runtime.shutdown_background();
}

#[tokio::test]
async fn damaged_context_state_surfaces_error_and_rolls_back_unpersisted_mutations() {
    struct FixedSummaryProvider;
    impl Provider for FixedSummaryProvider {
        fn stream(&self, request: ModelRequest, _cancel: CancellationToken) -> AssistantStream {
            let sys = system_prompt_of(&request);
            if sys.contains("Summarize this completed conversation segment") {
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_text(&request.model, "磁盘损坏时不落内存的摘要"),
                }]));
            }
            let reply = format!("长回复触发后台摘要：{}", "正文内容".repeat(120));
            Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: assistant_text(&request.model, reply),
            }]))
        }
    }

    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("damaged-ctx-ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let db_path = temp.path().join("ctx.sqlite");
    let state_path = temp.path().join("context-maintenance.json");

    let mut model = Model::new("openai", "ctx-model");
    model.context_window = Some(32_768);
    model.max_output_tokens = Some(512);

    let runtime = Runtime::open(
        &db_path,
        &workspace,
        Arc::new(FixedSummaryProvider),
        model.clone(),
        RuntimeConfig::default(),
    )
    .unwrap();
    let session = runtime.create(Some("Damaged Context".into())).unwrap().id;
    runtime
        .prompt(&session, "短轮次初始化状态文件", CancellationToken::new())
        .await
        .unwrap();
    runtime.shutdown_background();
    drop(runtime);

    // Corrupt the persisted context-maintenance.json file on disk.
    std::fs::write(&state_path, b"{corrupted json").unwrap();

    let reopened = Runtime::open(
        &db_path,
        &workspace,
        Arc::new(FixedSummaryProvider),
        model,
        RuntimeConfig::default(),
    )
    .unwrap();

    let initial_status = reopened.context_maintenance_status(&session).unwrap();
    assert!(initial_status.damaged);
    assert!(
        initial_status
            .last_error
            .as_deref()
            .is_some_and(|err| err.contains("context-maintenance.json")),
        "corrupted context-maintenance.json must surface in context_maintenance_status: {initial_status:?}"
    );

    // Running a new turn must NOT silently retain unpersisted summaries in memory while persistence is refused!
    reopened
        .prompt(
            &session,
            &format!("损坏后的新轮次 {}", "扩展输入".repeat(50)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        reopened.context_summaries(&session).unwrap().is_empty(),
        "unpersisted summary mutations must roll back when context-maintenance.json is damaged"
    );
    let rpc_status = reopened
        .call(
            "session.maintenance.status",
            json!({"session_id": session}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(rpc_status["damaged"], true);
    assert!(
        rpc_status["last_error"]
            .as_str()
            .is_some_and(|err| err.contains("context-maintenance.json")),
        "session.maintenance.status RPC must surface persistence failure: {rpc_status}"
    );

    reopened.shutdown_background();
}

#[tokio::test]
async fn memory_entry_id_accepts_legal_247_byte_apfs_upper_bound_and_work_tools_remove_artificial_caps()
 {
    let legal_max_id = format!(
        "m{}",
        "a".repeat(cayw_runtime::MAX_MEMORY_ENTRY_ID_BYTES - 1)
    );
    assert_eq!(legal_max_id.len(), 247);
    let over_max_id = format!("m{}", "a".repeat(cayw_runtime::MAX_MEMORY_ENTRY_ID_BYTES));
    assert_eq!(over_max_id.len(), 248);

    struct ToolScriptProvider {
        legal_max_id: String,
        over_max_id: String,
    }

    impl Provider for ToolScriptProvider {
        fn stream(&self, request: ModelRequest, _cancel: CancellationToken) -> AssistantStream {
            let last_non_system = request
                .messages
                .iter()
                .rev()
                .find(|m| !matches!(m, Message::System(_)));
            if let Some(Message::ToolResult(result)) = last_non_system {
                assert!(
                    !result.is_error,
                    "CodeMode tool execution failed: {}",
                    Message::ToolResult(result.clone()).text()
                );
                let page = result
                    .structured_content
                    .as_ref()
                    .and_then(|v| serde_json::from_value::<cayw_cells::CellPage>(v.clone()).ok());
                if let Some(p) = &page {
                    assert!(
                        !matches!(p.status, cayw_cells::CellStatus::Failed),
                        "Luau cell failed: {p:?}"
                    );
                }
                if page
                    .as_ref()
                    .is_some_and(|p| p.vm_alive || !p.status.is_terminal() || p.has_more)
                {
                    let mut msg = script_response(
                        &request,
                        "",
                        &format!("bounds-wait-{}", request.messages.len()),
                    );
                    msg.stop_reason = StopReason::ToolUse;
                    return Box::pin(futures::stream::iter([AssistantEvent::Done {
                        message: msg,
                    }]));
                }
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_text(&request.model, "已完成边界验证"),
                }]));
            }
            let last_text = last_non_system.map(Message::text).unwrap_or_default();
            let Some(session_log_id) = last_text.strip_prefix("SEED_LOG_ID:").map(str::trim) else {
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_text(&request.model, "已记录首轮输入"),
                }]));
            };
            let session_log_id = session_log_id.to_owned();
            let script = format!(
                r#"
local applied = cayw.call('memory.apply', {{
  candidates = {{
    {{
      operation = 'upsert',
      entry_id = '{legal_id}',
      kind = 'preference',
      body = '247字节合法文件名上限通过',
      source_type = 'stated',
      source_log_ids = {{ '{log_id}' }}
    }}
  }}
}})
assert(#applied.written == 1 and applied.written[1] == '{legal_id}', '247-byte entry_id must pass')

local ok_over, err_over = pcall(function()
  return cayw.call('memory.apply', {{
    candidates = {{
      {{
        operation = 'upsert',
        entry_id = '{over_id}',
        kind = 'preference',
        body = '248字节超过APFS临时文件上限',
        source_type = 'stated',
        source_log_ids = {{ '{log_id}' }}
      }}
    }}
  }})
end)
assert(not ok_over, '248-byte entry_id must be rejected by schema/storage')

local criteria = {{}}
local refs = {{}}
for i = 1, 70 do
  criteria[i] = {{
    criterion_id = 'crit-' .. tostring(i),
    description = 'criterion ' .. tostring(i),
    check = {{ kind = 'file_contains', path = 'big.log', text = 'TAIL_SENTINEL_OK' }}
  }}
  refs[i] = '{log_id}'
end
local created = cayw.call('work.create', {{
  work_id = 'work-70-items',
  title = '70项条件与超过8MiB文件验收',
  goal = '不设64项与8MiB拍脑袋限制',
  acceptance_criteria = criteria,
  user_source_refs = refs
}})
assert(#created.acceptance_criteria == 70, '70 criteria must be stored without truncation')
local ok_review, err_review = pcall(function()
  return cayw.call('work.accept.program', {{
    work_id = 'work-70-items',
    criterion_id = 'crit-70'
  }})
end)
assert(not ok_review, 'the main session must not review: ' .. tostring(err_review))
return {{ written = applied.written[1], count = #created.acceptance_criteria }}
"#,
                legal_id = self.legal_max_id,
                over_id = self.over_max_id,
                log_id = session_log_id,
            );
            let mut msg = script_response(&request, &script, "bounds-call");
            msg.stop_reason = StopReason::ToolUse;
            Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: msg,
            }]))
        }
    }

    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("bounds-ws");
    std::fs::create_dir_all(&workspace).unwrap();

    // Write an > 8 MiB file with `TAIL_SENTINEL_OK` at the end.
    let mut big_bytes = vec![b'x'; 8 * 1024 * 1024 + 1024];
    big_bytes.extend_from_slice(b"\nTAIL_SENTINEL_OK\n");
    std::fs::write(workspace.join("big.log"), &big_bytes).unwrap();

    let runtime = Runtime::open(
        temp.path().join("bounds.sqlite"),
        &workspace,
        Arc::new(ToolScriptProvider {
            legal_max_id: legal_max_id.clone(),
            over_max_id: over_max_id.clone(),
        }),
        Model::new("fixture", "bounds"),
        RuntimeConfig {
            main_session: true,
            approval_rules: Some(Arc::new(FileRules::new(temp.path().join("rules.json")))),
            ..Default::default()
        },
    )
    .unwrap();
    let session = runtime.create(Some("Bounds".into())).unwrap().id;

    // First turn seeds a verified user message cursor in Core events.
    runtime
        .prompt(
            &session,
            "请记住247字节上限与大文件验收可以通过",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let user_cursor = runtime
        .events(&session, 0, 64)
        .unwrap()
        .into_iter()
        .find_map(|rec| match rec.event {
            cayw_core::CoreEvent::Agent { event } => match event.as_ref() {
                cayw_agent::AgentEvent::MessageEnd {
                    message: Message::User { .. },
                } => Some(rec.cursor),
                _ => None,
            },
            _ => None,
        })
        .expect("seeded user cursor");
    let seed_log_id = format!("{session}:{user_cursor}");

    // Also verify direct `MainMemory` rejection of 248 bytes names `247`.
    let over_err = runtime
        .apply_memory_candidates(
            &session,
            &[MemoryCandidate {
                operation: MemoryOperation::Upsert,
                entry_id: Some(over_max_id),
                kind: "preference".into(),
                body: "too long".into(),
                source_type: SourceType::Stated,
                source_log_ids: vec![seed_log_id.clone()],
            }],
        )
        .unwrap();
    assert_eq!(over_err.rejected.len(), 1);
    assert!(
        over_err.rejected[0].reason.contains("247"),
        "rejection reason must cite the 247-byte APFS limit: {:?}",
        over_err.rejected[0]
    );

    // Second turn stores the 247-byte entry and the 70 criteria. The main session
    // does not review; the child session below reads the large file.
    runtime
        .prompt(
            &session,
            &format!("SEED_LOG_ID:{seed_log_id}"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let entries = runtime.memory_entries().unwrap();
    assert!(
        entries.iter().any(|e| e.entry_id == legal_max_id),
        "247-byte entry_id must be persisted on disk and visible: {entries:?}"
    );
    assert!(
        workspace
            .join("memory")
            .join(format!("{legal_max_id}.md"))
            .is_file()
    );
    let item = runtime.work_item("work-70-items").unwrap().unwrap();
    assert_eq!(item.acceptance_criteria.len(), 70);
    assert_eq!(item.user_source_refs.len(), 70);
    assert!(
        item.acceptance_records.is_empty(),
        "the main session must not record a program check"
    );

    struct BoundsOwner {
        main: Runtime,
    }

    #[async_trait]
    impl WorkOwner for BoundsOwner {
        async fn work_item(
            &self,
            work_id: &str,
        ) -> Result<cayw_runtime::WorkItem, WorkRouterError> {
            self.main
                .work_item(work_id)
                .map_err(|error| WorkRouterError::known(error.to_string()))?
                .ok_or_else(|| WorkRouterError::known("WorkItem not found"))
        }

        async fn record_program_acceptance(
            &self,
            work_id: &str,
            criterion_id: &str,
            passed: bool,
            evidence: String,
        ) -> Result<cayw_runtime::WorkItem, WorkRouterError> {
            self.main
                .record_child_program_acceptance(work_id, criterion_id, passed, evidence)
                .map_err(|error| WorkRouterError::known(error.to_string()))
        }

        async fn report_status(
            &self,
            report: WorkStatusReport,
        ) -> Result<cayw_runtime::WorkSessionNoticeReply, WorkRouterError> {
            self.main
                .relay_task_status(report)
                .map_err(|error| WorkRouterError::known(error.to_string()))
        }
    }

    struct ChildReview;

    impl Provider for ChildReview {
        fn stream(&self, request: ModelRequest, _: CancellationToken) -> AssistantStream {
            let message = script_response(
                &request,
                r#"local evaluated = cayw.call('work.accept.program', {work_id='work-70-items', criterion_id='crit-70'})
assert(evaluated.passed == true, 'over-8MiB file check must pass')"#,
                &format!("child-review-{}", request.messages.len()),
            );
            Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
        }
    }

    let child = Runtime::open(
        temp.path().join("child-bounds.sqlite"),
        &workspace,
        Arc::new(ChildReview),
        Model::new("fixture", "child-bounds"),
        RuntimeConfig {
            approval_rules: Some(Arc::new(FileRules::new(
                temp.path().join("child-rules.json"),
            ))),
            ..Default::default()
        },
    )
    .unwrap();
    child
        .set_work_owner(Arc::new(BoundsOwner {
            main: runtime.clone(),
        }))
        .unwrap();
    let child_session = child.create(Some("Reviewer".into())).unwrap().id;
    let dispatched = child
        .enqueue_dispatched(
            &child_session,
            TaskInput {
                text: "review crit-70".into(),
                images: Vec::new(),
            },
            TaskIntent::General,
            TaskPriority::Normal,
            TaskResources::default(),
            TaskDispatch {
                work_id: item.work_id.clone(),
                dispatch_key: "review-crit-70".into(),
                pause_version: item.pause_version,
                stop_version: item.stop_version,
            },
        )
        .unwrap();
    child
        .resume(&dispatched.id, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        child.task(&dispatched.id).unwrap().state,
        TaskState::Completed
    );

    let reviewed = runtime.work_item("work-70-items").unwrap().unwrap();
    assert_eq!(reviewed.acceptance_records.len(), 1);
    assert_eq!(
        reviewed.acceptance_records[0].result,
        AcceptanceResult::Passed
    );
    assert_eq!(
        reviewed.acceptance_records[0].source,
        AcceptanceSource::Program
    );
    assert_eq!(reviewed.acceptance_records[0].writer, "child-session");

    runtime.shutdown_background();
    child.shutdown_background();
}

#[tokio::test]
async fn proactive_auto_validates_preconditions_before_recording_trigger_and_returns_task_id() {
    struct ExternalRouter {
        target: Runtime,
    }

    #[async_trait]
    impl WorkRouter for ExternalRouter {
        async fn workspaces(&self) -> Result<Vec<Value>, WorkRouterError> {
            Ok(vec![json!({
                "id": "ext-ws",
                "path": self.target.workspace().to_string_lossy(),
                "state": "open"
            })])
        }
        async fn sessions(&self, _workspace: &str) -> Result<Vec<Value>, WorkRouterError> {
            Ok(vec![])
        }
        async fn create_session(
            &self,
            _workspace: &str,
            label: &str,
        ) -> Result<Value, WorkRouterError> {
            let s = self
                .target
                .create(Some(label.to_owned()))
                .map_err(|e| WorkRouterError::known(e.to_string()))?;
            serde_json::to_value(s).map_err(|e| WorkRouterError::known(e.to_string()))
        }
        async fn dispatch(
            &self,
            request: WorkDispatchRequest,
        ) -> Result<WorkDispatchReply, WorkRouterError> {
            let session_id = match request.session_id {
                Some(id) => id,
                None => self.target.create(Some("Proactive".into())).unwrap().id,
            };
            let reused = self
                .target
                .task_by_dispatch_key(&request.dispatch_key)
                .map_err(|e| WorkRouterError::known(e.to_string()))?
                .is_some();
            let task = self
                .target
                .enqueue_dispatched(
                    &session_id,
                    TaskInput {
                        text: request.text,
                        images: Vec::new(),
                    },
                    TaskIntent::General,
                    request.priority,
                    TaskResources::default(),
                    TaskDispatch {
                        work_id: request.work_id.clone(),
                        dispatch_key: request.dispatch_key.clone(),
                        pause_version: request.pause_version,
                        stop_version: request.stop_version,
                    },
                )
                .map_err(|e| WorkRouterError::known(e.to_string()))?;
            Ok(WorkDispatchReply {
                workspace: request.workspace,
                session_id,
                task_id: task.id,
                state: task.state,
                work_id: request.work_id,
                dispatch_key: request.dispatch_key,
                reused,
            })
        }
        async fn task(
            &self,
            query: WorkTaskQuery,
        ) -> Result<Option<cayw_runtime::TaskRecord>, WorkRouterError> {
            if let Some(key) = query.dispatch_key.as_deref() {
                return self
                    .target
                    .task_by_dispatch_key(key)
                    .map_err(|e| WorkRouterError::known(e.to_string()));
            }
            if let Some(id) = query.task_id.as_deref() {
                return self
                    .target
                    .task(id)
                    .map(Some)
                    .map_err(|e| WorkRouterError::known(e.to_string()));
            }
            Ok(None)
        }
        async fn control(
            &self,
            _action: WorkControl,
            _work_id: &str,
            _version: u64,
            _workspaces: &[String],
        ) -> Result<Vec<WorkTargetOutcome>, WorkRouterError> {
            Ok(vec![])
        }
        async fn evaluate_check(
            &self,
            _workspace: &str,
            _check: &AcceptanceCheck,
        ) -> Result<bool, WorkRouterError> {
            Ok(false)
        }

        async fn notify_session(
            &self,
            notice: cayw_runtime::WorkSessionNotice,
        ) -> Result<cayw_runtime::WorkSessionNoticeReply, WorkRouterError> {
            self.target
                .deliver_session_notice(&notice)
                .map_err(|error| WorkRouterError::known(error.to_string()))
        }
    }

    let temp = TempDir::new().unwrap();
    let main_ws = temp.path().join("main-ws");
    let ext_ws = temp.path().join("ext-ws");
    std::fs::create_dir_all(&main_ws).unwrap();
    std::fs::create_dir_all(&ext_ws).unwrap();

    let target = Runtime::open(
        temp.path().join("ext.sqlite"),
        &ext_ws,
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "ext"),
        RuntimeConfig::default(),
    )
    .unwrap();
    let ext_session = target.create(Some("Ext Worker".into())).unwrap().id;

    let main_rt = Runtime::open(
        temp.path().join("main.sqlite"),
        &main_ws,
        Arc::new(cayw_runtime::fixture::OfflineProvider::default()),
        Model::new("fixture", "main"),
        RuntimeConfig {
            main_session: true,
            proactive_mode: ProactiveMode::Auto,
            approval_rules: Some(Arc::new(FileRules::new(temp.path().join("rules.json")))),
            ..Default::default()
        },
    )
    .unwrap();
    let main_session = main_rt.create(Some("Main".into())).unwrap().id;

    // 1. Paused WorkItem: Auto trigger fails BEFORE recording `(rule_id, condition_version)`.
    let local_work = main_rt
        .create_work_item(WorkItemSpec {
            work_id: Some("work-paused-precheck".into()),
            owner_main_session_id: main_session.clone(),
            title: "Paused check".into(),
            goal: "do not burn trigger key while paused".into(),
            acceptance_criteria: vec![AcceptanceCriterion {
                criterion_id: "c1".into(),
                description: "check".into(),
                check: None,
            }],
            user_source_refs: vec![format!("{main_session}:1")],
            action_scope: vec![],
            approval_refs: vec![],
            session_refs: vec![],
            task_refs: vec![],
            artifact_refs: vec![],
            waiting_conditions: vec![],
            pending_decisions: vec![],
            next_check_at: None,
        })
        .unwrap();
    main_rt.pause_work_item(&local_work.work_id).unwrap();
    assert!(
        main_rt
            .trigger_proactive(
                ProactiveMode::Auto,
                "rule-paused",
                1,
                &local_work.work_id,
                0
            )
            .is_err(),
        "Auto trigger on paused WorkItem must return an explicit error"
    );

    // 2. External-workspace WorkItem without `work_router`: fails BEFORE burning `("rule-ext", 1)`.
    let ext_work = main_rt
        .create_work_item(WorkItemSpec {
            work_id: Some("work-ext-precheck".into()),
            owner_main_session_id: main_session.clone(),
            title: "External check".into(),
            goal: "do not burn trigger key when work_router is absent".into(),
            acceptance_criteria: vec![AcceptanceCriterion {
                criterion_id: "c1".into(),
                description: "check".into(),
                check: None,
            }],
            user_source_refs: vec![format!("{main_session}:1")],
            action_scope: vec![],
            approval_refs: vec![],
            session_refs: vec![cayw_runtime::WorkSessionRef {
                session_id: ext_session.clone(),
                workspace: Some("ext-ws".into()),
            }],
            task_refs: vec![],
            artifact_refs: vec![],
            waiting_conditions: vec![],
            pending_decisions: vec![],
            next_check_at: None,
        })
        .unwrap();
    assert!(
        main_rt
            .call(
                "proactive.trigger",
                json!({
                    "mode": "auto",
                    "rule_id": "rule-ext",
                    "condition_version": 1,
                    "work_id": ext_work.work_id,
                }),
                CancellationToken::new(),
            )
            .await
            .is_err(),
        "external proactive trigger without work_router must fail without burning trigger key"
    );

    // Attach `work_router` and retry the EXACT SAME `("rule-ext", 1)` -> succeeds as `dispatched` with `task_id`!
    main_rt
        .set_work_router(Arc::new(ExternalRouter {
            target: target.clone(),
        }))
        .unwrap();
    let first = main_rt
        .call(
            "proactive.trigger",
            json!({
                "mode": "auto",
                "rule_id": "rule-ext",
                "condition_version": 1,
                "work_id": ext_work.work_id,
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first["kind"], "dispatched", "first={first}");
    let dispatched_task_id = first["task_id"]
        .as_str()
        .expect("dispatched RPC outcome must include task_id")
        .to_owned();
    assert!(!dispatched_task_id.is_empty());

    // Duplicate call on `("rule-ext", 1)` also returns the same `task_id`!
    let second = main_rt
        .call(
            "proactive.trigger",
            json!({
                "mode": "auto",
                "rule_id": "rule-ext",
                "condition_version": 1,
                "work_id": ext_work.work_id,
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(second["kind"], "duplicate", "second={second}");
    assert_eq!(
        second["task_id"].as_str(),
        Some(dispatched_task_id.as_str())
    );

    main_rt.shutdown_background();
    target.shutdown_background();
}

#[tokio::test]
async fn background_memory_writer_does_not_recreate_deleted_entry_and_queues_consecutive_session_updates()
 {
    #[derive(Default)]
    struct WriterRaceState {
        hold_next_writer: AtomicBool,
        writer_entered: Notify,
        release_writer: Notify,
        active_writers: AtomicUsize,
        max_concurrent_writers: AtomicUsize,
        writer_prompts: Mutex<Vec<String>>,
    }

    struct RaceProvider(Arc<WriterRaceState>);

    fn parse_turn_log_ids(prompt: &str) -> Vec<(String, String)> {
        let Some((_, turns_section)) = prompt.split_once("New user-input turns:\n") else {
            return Vec::new();
        };
        turns_section
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix('[')?;
                let (id, text) = rest.split_once("] ")?;
                Some((id.to_owned(), text.to_owned()))
            })
            .collect()
    }

    fn parse_first_existing_entry_id(prompt: &str) -> Option<String> {
        prompt.lines().find_map(|line| {
            let rest = line.strip_prefix("- entry_id=")?;
            let (entry_id, _) = rest.split_once(' ')?;
            Some(entry_id.to_owned())
        })
    }

    impl Provider for RaceProvider {
        fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> AssistantStream {
            let sys = system_prompt_of(&request);
            if !sys.contains("You are the main-session memory writer") {
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_text(&request.model, "前台已立即响应"),
                }]));
            }
            let state = self.0.clone();
            let prompt = request
                .messages
                .last()
                .map(Message::text)
                .unwrap_or_default();
            Box::pin(futures::stream::once(async move {
                let concurrent = state.active_writers.fetch_add(1, Ordering::AcqRel) + 1;
                state
                    .max_concurrent_writers
                    .fetch_max(concurrent, Ordering::AcqRel);
                state.writer_prompts.lock().unwrap().push(prompt.clone());
                let hold_this = state.hold_next_writer.swap(false, Ordering::AcqRel);
                state.writer_entered.notify_waiters();
                if hold_this {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            state.active_writers.fetch_sub(1, Ordering::AcqRel);
                            return AssistantEvent::Error {
                                message: AssistantMessage::new(&request.model)
                                    .failure(StopReason::Aborted, "cancelled"),
                            };
                        }
                        () = state.release_writer.notified() => {}
                    }
                }
                let turns = parse_turn_log_ids(&prompt);
                let existing_id = parse_first_existing_entry_id(&prompt);
                let mut candidates = Vec::new();
                for (log_id, text) in &turns {
                    if text.contains("初始语言英文") {
                        candidates.push(json!({
                            "operation": "upsert",
                            "entry_id": "mem-lang",
                            "kind": "preference",
                            "body": "默认使用英文回答",
                            "source_type": "stated",
                            "source_log_ids": [log_id],
                        }));
                    } else if text.contains("旧任务尝试更新已删条目") {
                        candidates.push(json!({
                            "operation": "upsert",
                            "entry_id": existing_id.clone().unwrap_or_else(|| "mem-lang".into()),
                            "kind": "preference",
                            "body": "旧后台任务试图写回已删除条目",
                            "source_type": "stated",
                            "source_log_ids": [log_id],
                        }));
                    } else if text.contains("默认使用法文回答") {
                        candidates.push(json!({
                            "operation": "upsert",
                            "entry_id": null,
                            "kind": "preference",
                            "body": "默认使用法文回答",
                            "source_type": "stated",
                            "source_log_ids": [log_id],
                        }));
                    } else if text.contains("统一改为默认使用中文回答") {
                        candidates.push(json!({
                            "operation": "upsert",
                            "entry_id": existing_id.clone(),
                            "kind": "preference",
                            "body": "以后统一改为默认使用中文回答",
                            "source_type": "stated",
                            "source_log_ids": [log_id],
                        }));
                    } else if text.contains("测试优先使用离线夹具") {
                        candidates.push(json!({
                            "operation": "upsert",
                            "entry_id": null,
                            "kind": "preference",
                            "body": "测试优先使用离线夹具",
                            "source_type": "stated",
                            "source_log_ids": [log_id],
                        }));
                    }
                }
                let payload = json!({
                    "candidates": candidates,
                    "forget": []
                })
                .to_string();
                state.active_writers.fetch_sub(1, Ordering::AcqRel);
                AssistantEvent::Done {
                    message: assistant_text(&request.model, payload),
                }
            }))
        }
    }

    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("writer-race-ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let state = Arc::new(WriterRaceState::default());

    let mut model = Model::new("openai", "writer-race-model");
    model.context_window = Some(32_768);
    model.max_output_tokens = Some(1_024);

    let runtime = Runtime::open(
        temp.path().join("writer-race.sqlite"),
        &workspace,
        Arc::new(RaceProvider(state.clone())),
        model,
        RuntimeConfig {
            main_session: true,
            background_model_maintenance: true,
            approval_rules: Some(Arc::new(FileRules::new(temp.path().join("rules.json")))),
            ..Default::default()
        },
    )
    .unwrap();
    let session = runtime.create(Some("Main Writer Queue".into())).unwrap().id;

    // 1. Seed `mem-lang.md` via Turn 1's background writer.
    runtime
        .prompt(&session, "请记住：初始语言英文", CancellationToken::new())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if runtime
            .memory_entries()
            .unwrap()
            .iter()
            .any(|e| e.entry_id == "mem-lang")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for initial mem-lang creation"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    let mem_lang_path = workspace.join("memory").join("mem-lang.md");
    assert!(mem_lang_path.is_file());

    // 2. Start Turn 2 while holding its background writer in flight after it snapshotted `mem-lang`.
    state.hold_next_writer.store(true, Ordering::Release);
    let entered_fut = state.writer_entered.notified();
    runtime
        .prompt(
            &session,
            "补充说明：旧任务尝试更新已删条目",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    entered_fut.await;

    // Delete `mem-lang.md` on disk while the background write is in flight, then release the writer.
    std::fs::remove_file(&mem_lang_path).unwrap();
    let frontier_before_release = runtime.memory().writer_frontier(&session).unwrap();
    state.release_writer.notify_one();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if runtime.memory().writer_frontier(&session).unwrap() > frontier_before_release {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for overlapping-delete writer frontier advance"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(
        !mem_lang_path.exists(),
        "stale background write must NOT recreate an entry deleted while the write was in flight"
    );
    assert!(runtime.memory_entries().unwrap().is_empty());
    assert!(runtime.pending_memory_candidates().unwrap().is_empty());

    // 3. Consecutive updates on the same session queue in the background without blocking foreground prompts.
    state.hold_next_writer.store(true, Ordering::Release);
    let turn3_entered = state.writer_entered.notified();
    runtime
        .prompt(
            &session,
            "请记住：默认使用法文回答",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    turn3_entered.await;

    // Submit 10 consecutive foreground turns while Turn 3's background writer is still blocked in flight:
    // all 10 foreground turns complete immediately without waiting, enter the session queue, and keep
    // background concurrency strictly at 1!
    let mut queued_turns = vec!["更正：以后统一改为默认使用中文回答".to_owned()];
    for idx in 1..=8 {
        queued_turns.push(format!("临时对话轮次-{idx}"));
    }
    queued_turns.push("补充：测试优先使用离线夹具".to_owned());
    assert_eq!(queued_turns.len(), 10);
    for turn_text in &queued_turns {
        runtime
            .prompt(&session, turn_text, CancellationToken::new())
            .await
            .unwrap();
    }
    assert_eq!(
        state.max_concurrent_writers.load(Ordering::Acquire),
        1,
        "same-session background memory writes must serialize rather than run overlapping model calls"
    );

    // Release Turn 3's writer: Turn 3 commits French, then the queued run starts with the latest
    // `writer_frontier` and Turn 3's committed entry, updating it to Chinese cleanly with zero pending conflicts!
    state.release_writer.notify_one();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let entries = runtime.memory_entries().unwrap();
        let has_chinese = entries
            .iter()
            .any(|e| e.body.contains("以后统一改为默认使用中文回答"));
        let has_fixture = entries
            .iter()
            .any(|e| e.body.contains("测试优先使用离线夹具"));
        if has_chinese && has_fixture {
            assert_eq!(
                entries.len(),
                2,
                "Turn 4 correction must update Turn 3's entry in place rather than leaving stale French: {entries:?}"
            );
            assert!(
                !entries.iter().any(|e| e.body.contains("默认使用法文回答")),
                "superseded preference from Turn 3 must be updated by queued Turn 4: {entries:?}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for queued consecutive memory updates; entries={entries:?}, pending={:?}",
            runtime.pending_memory_candidates().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(
        runtime.pending_memory_candidates().unwrap().is_empty(),
        "queued same-session correction must read latest version and avoid version-conflict pending state"
    );
    assert_eq!(state.max_concurrent_writers.load(Ordering::Acquire), 1);

    let prompts = state.writer_prompts.lock().unwrap().clone();
    let last_prompt = prompts.last().expect("queued writer prompt");
    assert!(
        !last_prompt.contains("默认使用法文回答\n")
            || last_prompt.contains("Existing active memory entries:\n- entry_id="),
        "queued writer must see Turn 3's active entry in existing entries: {last_prompt}"
    );
    let (_, queued_turns_section) = last_prompt
        .split_once("New user-input turns:\n")
        .expect("turns section");
    assert!(
        !queued_turns_section.contains("请记住：默认使用法文回答"),
        "queued writer must read updated writer_frontier and not re-process Turn 3: {queued_turns_section}"
    );
    assert!(queued_turns_section.contains("更正：以后统一改为默认使用中文回答"));
    assert!(queued_turns_section.contains("补充：测试优先使用离线夹具"));

    runtime.shutdown_background();
}

#[tokio::test]
async fn length_stop_reason_does_not_discard_valid_summary_compression_or_memory_json() {
    struct LengthStopProvider {
        truncate_writer_once: AtomicBool,
    }

    fn assistant_length_text(model: &Model, text: impl Into<String>) -> AssistantMessage {
        let mut msg = assistant_text(model, text);
        msg.stop_reason = StopReason::Length;
        msg
    }

    impl Provider for LengthStopProvider {
        fn stream(&self, request: ModelRequest, _cancel: CancellationToken) -> AssistantStream {
            let sys = system_prompt_of(&request);
            if sys.contains("Summarize this completed conversation segment") {
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_length_text(
                        &request.model,
                        "触及输出上限但内容完整的有效分段摘要。",
                    ),
                }]));
            }
            if sys.contains("Compress this historical session prefix") {
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_length_text(
                        &request.model,
                        "触及输出上限但预算与内容均有效的历史压缩文本。",
                    ),
                }]));
            }
            if sys.contains("You are the main-session memory writer") {
                if self.truncate_writer_once.swap(false, Ordering::AcqRel) {
                    return Box::pin(futures::stream::iter([AssistantEvent::Done {
                        message: assistant_length_text(
                            &request.model,
                            "{\"candidates\":[{\"kind\":\"preference\"",
                        ),
                    }]));
                }
                let prompt = request
                    .messages
                    .last()
                    .map(Message::text)
                    .unwrap_or_default();
                let log_id = prompt
                    .lines()
                    .find_map(|line| {
                        let rest = line.strip_prefix('[')?;
                        let (id, _) = rest.split_once(']')?;
                        Some(id.to_owned())
                    })
                    .expect("writer log_id");
                let payload = json!({
                    "candidates": [{
                        "operation": "upsert",
                        "entry_id": null,
                        "kind": "preference",
                        "body": "结束原因为 Length 的完整 JSON 仍正常写入记忆",
                        "source_type": "stated",
                        "source_log_ids": [log_id],
                    }],
                    "forget": []
                })
                .to_string();
                return Box::pin(futures::stream::iter([AssistantEvent::Done {
                    message: assistant_length_text(&request.model, payload),
                }]));
            }
            let reply = format!("前台长回复：{}", "历史载荷填充文本".repeat(60));
            Box::pin(futures::stream::iter([AssistantEvent::Done {
                message: assistant_text(&request.model, reply),
            }]))
        }
    }

    let temp = TempDir::new().unwrap();

    // 1. Ordinary workspace: summary + compression with `StopReason::Length` still publish when valid.
    let work_ws = temp.path().join("length-work-ws");
    std::fs::create_dir_all(&work_ws).unwrap();
    let mut small_model = Model::new("openai", "length-compress-model");
    small_model.context_window = Some(2_200);
    small_model.max_output_tokens = Some(256);

    let work_rt = Runtime::open(
        temp.path().join("length-work.sqlite"),
        &work_ws,
        Arc::new(LengthStopProvider {
            truncate_writer_once: AtomicBool::new(false),
        }),
        small_model,
        RuntimeConfig::default(),
    )
    .unwrap();
    let work_session = work_rt.create(Some("Work Length".into())).unwrap().id;
    for idx in 1..=3 {
        work_rt
            .prompt(
                &work_session,
                &format!("第{idx}轮长输入：{}", "需要压缩的上下文记录".repeat(25)),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let summaries = work_rt.context_summaries(&work_session).unwrap();
        let maint = work_rt.context_maintenance_status(&work_session).unwrap();
        if summaries.iter().any(|s| !s.local_reuse) && maint.compressions > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "valid summary/compression with StopReason::Length must not be discarded; summaries={summaries:?}, maint={maint:?}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    // 2. Main session: truncated JSON on `Length` fails JSON validation without advancing frontier,
    // while complete JSON on `Length` succeeds and writes memory.
    let main_ws = temp.path().join("length-main-ws");
    std::fs::create_dir_all(&main_ws).unwrap();
    let mut main_model = Model::new("openai", "length-main-model");
    main_model.context_window = Some(32_768);
    main_model.max_output_tokens = Some(512);

    let main_rt = Runtime::open(
        temp.path().join("length-main.sqlite"),
        &main_ws,
        Arc::new(LengthStopProvider {
            truncate_writer_once: AtomicBool::new(true),
        }),
        main_model,
        RuntimeConfig {
            main_session: true,
            background_model_maintenance: true,
            approval_rules: Some(Arc::new(FileRules::new(temp.path().join("rules.json")))),
            ..Default::default()
        },
    )
    .unwrap();
    let main_session = main_rt.create(Some("Main Length".into())).unwrap().id;
    main_rt
        .prompt(
            &main_session,
            "请记住：结束原因为 Length 但 JSON 完整时不应丢弃",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if main_rt.memory().status().unwrap().writer_failures == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "truncated JSON must be caught by JSON parser"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert_eq!(main_rt.memory().writer_frontier(&main_session).unwrap(), 0);

    main_rt
        .prompt(&main_session, "继续下一步", CancellationToken::new())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if main_rt.memory_entries().unwrap().iter().any(|e| {
            e.body
                .contains("结束原因为 Length 的完整 JSON 仍正常写入记忆")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "complete JSON with StopReason::Length must be accepted"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    work_rt.shutdown_background();
    main_rt.shutdown_background();
}

#[tokio::test]
async fn concurrent_memory_scan_during_stated_writes_never_reclassifies_entries_as_user_set() {
    let temp = TempDir::new().unwrap();
    let ws = temp.path().join("race-main-ws");
    std::fs::create_dir_all(&ws).unwrap();
    let memory = Arc::new(cayw_runtime::MainMemory::open(&ws));
    let stop = Arc::new(AtomicBool::new(false));

    let scanner_memory = memory.clone();
    let scanner_stop = stop.clone();
    let scanner = std::thread::spawn(move || {
        while !scanner_stop.load(Ordering::Acquire) {
            let status = scanner_memory.status().unwrap();
            assert_eq!(
                status.user_set, 0,
                "concurrent scan must never reclassify background stated entries as UserSet: {status:?}"
            );
            let _ = scanner_memory.visible_entries().unwrap();
        }
    });

    for idx in 1..=20_u64 {
        let cursor = idx * 2;
        let log_id = format!("race-session:{cursor}");
        let events = vec![cayw_core::EventRecord {
            protocol_version: 3,
            session_id: "race-session".into(),
            cursor,
            timestamp: idx,
            attempt: None,
            event: cayw_core::CoreEvent::Agent {
                event: Box::new(cayw_agent::AgentEvent::MessageStart {
                    message: Message::user(format!("请记住第{idx}条事实")),
                }),
            },
        }];
        let evidence = cayw_runtime::user_evidence(&events);
        let candidate = MemoryCandidate {
            operation: MemoryOperation::Upsert,
            entry_id: Some(format!("stated-entry-{idx}")),
            kind: "preference".into(),
            body: format!("口述内容 {idx}"),
            source_type: SourceType::Stated,
            source_log_ids: vec![log_id],
        };
        let report = memory.apply(&evidence, &[candidate]).unwrap();
        assert_eq!(report.written.len(), 1);
        assert!(report.rejected.is_empty());
    }

    stop.store(true, Ordering::Release);
    scanner.join().unwrap();

    // Reopen MainMemory from disk to ensure no UserSet event was persisted to events.jsonl.
    let reopened = cayw_runtime::MainMemory::open(&ws);
    let status = reopened.status().unwrap();
    assert_eq!(status.entries, 20);
    assert_eq!(status.active, 20);
    assert_eq!(status.visible, 20);
    assert_eq!(status.user_set, 0);
    for entry in reopened.visible_entries().unwrap() {
        assert_eq!(entry.source_type, SourceType::Stated);
    }
}

async fn spawn_openai_echo_sse_server(
    captured_requests: Arc<Mutex<Vec<Value>>>,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut seq = 0_u64;
        while let Ok((mut socket, _)) = listener.accept().await {
            let captured = captured_requests.clone();
            seq += 1;
            let resp_id = format!("echo-{seq}");
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut buf = [0_u8; 4096];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let len = headers
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + len {
                    let mut buf = [0_u8; 4096];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                let body: Value =
                    serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap();
                captured.lock().unwrap().push(body.clone());

                let sys_text = body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| m["role"] == "system")
                    .filter_map(|m| m["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let last_role = body["messages"]
                    .as_array()
                    .and_then(|m| m.last())
                    .and_then(|m| m["role"].as_str())
                    .unwrap_or("");
                let has_exec_tool = body["tools"].as_array().is_some_and(|tools| {
                    tools
                        .iter()
                        .any(|t| t["function"]["name"] == "execute_code")
                });
                let last_user_text = body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| {
                        m["content"].as_str().map(ToOwned::to_owned).or_else(|| {
                            m["content"]
                                .as_array()
                                .and_then(|parts| parts.first())
                                .and_then(|p| p["text"].as_str())
                                .map(ToOwned::to_owned)
                        })
                    })
                    .unwrap_or_default();

                let chunk = if sys_text.contains("You are the main-session memory writer") {
                    let payload = json!({"candidates": [], "forget": []}).to_string();
                    json!({
                        "id": resp_id,
                        "model": body["model"],
                        "choices": [{
                            "index": 0,
                            "delta": {"role": "assistant", "content": payload},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
                    })
                } else if sys_text.contains("Summarize this completed conversation segment")
                    || sys_text.contains("Compress this historical session prefix")
                {
                    json!({
                        "id": resp_id,
                        "model": body["model"],
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "content": "压缩摘要：早期多消息工具轮已被安全压缩，保留关键上下文。"
                            },
                            "finish_reason": "length"
                        }],
                        "usage": {"prompt_tokens": 16, "completion_tokens": 16, "total_tokens": 32}
                    })
                } else if has_exec_tool
                    && last_role == "user"
                    && last_user_text.contains("TRIGGER_TOOL_TURN")
                {
                    let args = json!({
                        "action": "exec",
                        "script": "return {status = 'tool-turn-done'}",
                        "yield_time_ms": 5000,
                        "max_output_tokens": 4096
                    });
                    json!({
                        "id": resp_id,
                        "model": body["model"],
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "tool_calls": [{
                                    "index": 0,
                                    "id": format!("call-{resp_id}"),
                                    "type": "function",
                                    "function": {
                                        "name": "execute_code",
                                        "arguments": args.to_string()
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }],
                        "usage": {"prompt_tokens": 20, "completion_tokens": 12, "total_tokens": 32}
                    })
                } else {
                    let reply = format!(
                        "已收到回复（{}）",
                        "填充模型历史窗口的稳定回复内容".repeat(12)
                    );
                    json!({
                        "id": resp_id,
                        "model": body["model"],
                        "choices": [{
                            "index": 0,
                            "delta": {"role": "assistant", "content": reply},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 20, "completion_tokens": 24, "total_tokens": 44}
                    })
                };

                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let http = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = socket.write_all(http.as_bytes()).await;
            });
        }
    });
    (base_url, handle)
}

#[tokio::test]
async fn multi_message_tool_turn_eviction_and_window_none_fallback_preserve_openai_turn_pairing_and_memory()
 {
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (base_url, server_handle) = spawn_openai_echo_sse_server(captured.clone()).await;
    let temp = TempDir::new().unwrap();

    // Part A: Tight context window with multi-message tool turn (`[User, Assistant(tool_call), ToolResult, Assistant(final)]`)
    // in both ordinary Runtime and main-session Runtime through real `OpenAiProvider`.
    // Previously, evicting `[User, Assistant(tool_call), ToolResult]` left a lone `Assistant(final)` after
    // `Piece::Compression`/`Piece::Summary`, causing `OpenAiProvider` to reject with
    // `"Assistant history needs a preceding user or tool-result turn"`.
    for (name, is_main) in [("ordinary", false), ("main", true)] {
        let ws = temp.path().join(format!("{name}-evict-ws"));
        std::fs::create_dir_all(&ws).unwrap();
        let provider: Arc<dyn Provider> =
            Arc::new(cayw_ai::providers::OpenAiProvider::new(&base_url, "synthetic-key").unwrap());
        let mut model = Model::new("openai", format!("{name}-evict-model"));
        model.context_window = Some(2_300);
        model.max_output_tokens = Some(256);

        let approval_rules: Option<Arc<dyn cayw_runtime::ApprovalRuleSource>> = if is_main {
            Some(Arc::new(FileRules::new(
                temp.path().join(format!("{name}-rules.json")),
            )))
        } else {
            None
        };

        let rt = Runtime::open(
            temp.path().join(format!("{name}-evict.sqlite")),
            &ws,
            provider,
            model,
            RuntimeConfig {
                main_session: is_main,
                background_model_maintenance: true,
                approval_rules,
                ..Default::default()
            },
        )
        .unwrap();
        let session = rt.create(Some(format!("{name}-session"))).unwrap().id;

        // Turn 1: multi-message Luau tool turn.
        rt.prompt(
            &session,
            &format!("TRIGGER_TOOL_TURN {}", "首轮工具调用长文本".repeat(16)),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        // Subsequent turns push Turn 1 partially/fully across the eviction boundary and wait for
        // background compression/summary (`StopReason::Length`) to publish, then run one more turn
        // with the published compression/summary active in the OpenAiProvider history!
        for turn in 2..=6 {
            rt.prompt(
                &session,
                &format!(
                    "第{turn}轮普通对话：{}",
                    "用于触发滑动窗口驱逐与摘要压缩的文本".repeat(18)
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|err| {
                panic!("{name} turn {turn} failed through real OpenAiProvider: {err}")
            });
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let maint = rt.context_maintenance_status(&session).unwrap();
            if maint.compressions > 0 || maint.summaries > 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{name} expected background maintenance to publish: {:?}",
                rt.context_maintenance_status(&session).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Turn 7 must succeed through real OpenAiProvider with active Compression/Summary pieces!
        rt.prompt(
            &session,
            "验证压缩块生效后真实 OpenAiProvider 仍通过消息配对校验",
            CancellationToken::new(),
        )
        .await
        .unwrap();

        rt.shutdown_background();
    }

    // Part B: `context_window: None` (`window: None` fallback in `main_memory.rs`) must:
    // 1. Still inject `memory.context_block()` into the system prompt;
    // 2. Keep valid `User`/`Assistant` turn pairing for `OpenAiProvider` even after `forget_memory_range`
    //    excludes an early user turn whose assistant reply remains in history.
    let none_ws = temp.path().join("window-none-main-ws");
    std::fs::create_dir_all(&none_ws).unwrap();
    let none_provider: Arc<dyn Provider> =
        Arc::new(cayw_ai::providers::OpenAiProvider::new(&base_url, "synthetic-key").unwrap());
    let mut none_model = Model::new("openai", "window-none-model");
    none_model.context_window = None;
    none_model.max_output_tokens = Some(256);

    let none_rt = Runtime::open(
        temp.path().join("window-none.sqlite"),
        &none_ws,
        none_provider,
        none_model,
        RuntimeConfig {
            main_session: true,
            background_model_maintenance: false,
            approval_rules: Some(Arc::new(FileRules::new(
                temp.path().join("none-rules.json"),
            ))),
            ..Default::default()
        },
    )
    .unwrap();
    let none_session = none_rt.create(Some("Window None".into())).unwrap().id;

    // Turn 1: establish a user preference turn we keep in memory, followed by Turn 2 whose user
    // message cursor ONLY is forgotten while its assistant reply remains outside the forgotten range.
    none_rt
        .prompt(
            &none_session,
            "请记住我的无窗口回退偏好",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let pref_evidence = none_rt.memory_user_evidence(&none_session).unwrap();
    let pref_log_id = pref_evidence
        .sources()
        .next()
        .expect("turn 1 user source")
        .log_id()
        .to_owned();
    none_rt
        .apply_memory_candidates(
            &none_session,
            &[MemoryCandidate {
                operation: MemoryOperation::Upsert,
                entry_id: Some("fallback-pref".into()),
                kind: "preference".into(),
                body: "即使未配置 context_window 也必须注入记忆块。".into(),
                source_type: SourceType::Stated,
                source_log_ids: vec![pref_log_id],
            }],
        )
        .unwrap();

    let cursor_before_turn2 = none_rt.get(&none_session).unwrap().cursor;
    none_rt
        .prompt(
            &none_session,
            "第二轮将被单独遗忘的用户输入",
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // Find the exact cursor span of Turn 2's user message (`MessageStart..=MessageEnd`) so ONLY
    // the user message events are forgotten while Turn 2's assistant reply remains non-forgotten!
    let events = none_rt
        .events(&none_session, cursor_before_turn2, 64)
        .unwrap();
    let user_cursors: Vec<u64> = events
        .iter()
        .filter(|ev| match &ev.event {
            cayw_core::CoreEvent::Agent { event } => matches!(
                event.as_ref(),
                cayw_agent::AgentEvent::MessageStart {
                    message: Message::User { .. }
                } | cayw_agent::AgentEvent::MessageEnd {
                    message: Message::User { .. }
                }
            ),
            _ => false,
        })
        .map(|ev| ev.cursor)
        .collect();
    let start_c = *user_cursors.first().expect("turn 2 user start cursor");
    let end_c = *user_cursors.last().expect("turn 2 user end cursor");
    none_rt
        .forget_memory_range(
            cayw_context::SourceRange::new(&none_session, start_c, end_c).unwrap(),
            "遗忘第二轮用户输入但保留助手回复以验证配对",
        )
        .unwrap();

    captured.lock().unwrap().clear();
    none_rt
        .prompt(
            &none_session,
            "第三轮验证无窗口回退",
            CancellationToken::new(),
        )
        .await
        .expect(
            "OpenAiProvider must accept history after forgetting only a preceding user message",
        );

    let reqs = captured.lock().unwrap().clone();
    let main_req = reqs
        .iter()
        .find(|r| r["model"] == "window-none-model")
        .expect("must capture window-none-model request");
    let sys_joined = main_req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        sys_joined.contains("[用户偏好]")
            && sys_joined.contains("即使未配置 context_window 也必须注入记忆块。"),
        "window: None fallback must include MainMemory context block, got: {sys_joined}"
    );

    none_rt.shutdown_background();
    server_handle.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn full_chain_serve_ipc_luau_cross_workspace_openai_sse_memory_and_context_maintenance() {
    use std::{os::unix::fs::PermissionsExt, process::Stdio};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        process::Command,
    };

    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    let custom_main_dir = home.join("custom-main-session");
    let project_alpha = temp.path().join("project-alpha");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir_all(&project_alpha).unwrap();

    let config_path = temp.path().join("device-config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&json!({
            "main_session_dir": custom_main_dir,
        }))
        .unwrap(),
    )
    .unwrap();

    // Start a real loopback HTTP SSE server speaking OpenAI `/v1/chat/completions`.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let project_alpha_id_slot = Arc::new(Mutex::new(String::new()));
    let project_alpha_for_sse = project_alpha_id_slot.clone();

    let sse_server = tokio::spawn(async move {
        let mut seq = 0_u64;
        while let Ok((mut socket, _)) = listener.accept().await {
            seq += 1;
            let resp_id = format!("fullchain-{seq}");
            let alpha_slot = project_alpha_for_sse.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut buf = [0_u8; 4096];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let len = headers
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + len {
                    let mut buf = [0_u8; 4096];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                let body: Value =
                    serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap();
                let sys_text = body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| m["role"] == "system")
                    .filter_map(|m| m["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let last_role = body["messages"]
                    .as_array()
                    .and_then(|m| m.last())
                    .and_then(|m| m["role"].as_str())
                    .unwrap_or("");
                let last_user_text = body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| {
                        m["content"].as_str().map(ToOwned::to_owned).or_else(|| {
                            m["content"]
                                .as_array()
                                .and_then(|parts| parts.first())
                                .and_then(|p| p["text"].as_str())
                                .map(ToOwned::to_owned)
                        })
                    })
                    .unwrap_or_default();

                let chunk = if sys_text.contains("You are the main-session memory writer") {
                    // Extract `[<log_id>] <text>` from the `New user-input turns:` section!
                    let turns_section = last_user_text
                        .split_once("New user-input turns:\n")
                        .map(|(_, rest)| rest)
                        .unwrap_or(&last_user_text);
                    let mut matched = None::<(String, &'static str, &'static str)>;
                    for line in turns_section.lines() {
                        if let Some(rest) = line.strip_prefix('[')
                            && let Some((log_id, text_part)) = rest.split_once(']')
                        {
                            if text_part.contains("MEM_FACT_ONE") {
                                matched = Some((
                                    log_id.to_owned(),
                                    "fullchain-fact-one",
                                    "用户偏好全链路真实进程验证（第一条）。",
                                ));
                            } else if text_part.contains("MEM_FACT_TWO") {
                                matched = Some((
                                    log_id.to_owned(),
                                    "fullchain-fact-two",
                                    "重启后口述记忆仍保持 Stated 来源（第二条）。",
                                ));
                            }
                        }
                    }
                    let payload = if let Some((log_id, id, fact_body)) = matched {
                        json!({
                            "candidates": [{
                                "operation": "upsert",
                                "entry_id": id,
                                "kind": "preference",
                                "body": fact_body,
                                "source_type": "stated",
                                "source_log_ids": [log_id]
                            }],
                            "forget": []
                        })
                        .to_string()
                    } else {
                        json!({"candidates": [], "forget": []}).to_string()
                    };
                    // Return with finish_reason="length" to verify valid JSON on Length is retained!
                    json!({
                        "id": resp_id,
                        "model": body["model"],
                        "choices": [{
                            "index": 0,
                            "delta": {"role": "assistant", "content": payload},
                            "finish_reason": "length"
                        }],
                        "usage": {"prompt_tokens": 24, "completion_tokens": 32, "total_tokens": 56}
                    })
                } else if sys_text.contains("Summarize this completed conversation segment")
                    || sys_text.contains("Compress this historical session prefix")
                {
                    json!({
                        "id": resp_id,
                        "model": body["model"],
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "content": "全链路压缩摘要：跨工作区派发与早期对话已完成归档。"
                            },
                            "finish_reason": "length"
                        }],
                        "usage": {"prompt_tokens": 30, "completion_tokens": 20, "total_tokens": 50}
                    })
                } else if last_role == "user"
                    && !last_user_text.contains("EXTERNAL_WORKER_TASK")
                    && (last_user_text.contains("DISPATCH_CROSS_WORKSPACE")
                        || last_user_text.contains('第')
                        || last_user_text.contains("MEM_FACT_TWO")
                        || last_user_text.contains("cannot finish yet"))
                {
                    let alpha_id = alpha_slot.lock().unwrap().clone();
                    let script = format!(
                        r#"
local listed = cayw.call('work.list', {{}})
local existing = nil
for _, item in ipairs(listed) do
  if item.work_id == 'work-fullchain-alpha' then
    existing = item
  end
end
if existing ~= nil then
  local ref = existing.session_refs[1]
  local dispatched = cayw.call('work.dispatch', {{
    work_id = existing.work_id,
    workspace = ref.workspace,
    session_id = ref.session_id,
    text = 'EXTERNAL_WORKER_TASK: 请返回子工作区执行结果',
    dispatch_key = 'dk-fullchain-alpha-1'
  }})
  return {{work_id = existing.work_id, alpha_session = ref.session_id, task_id = dispatched.task_id}}
end
local created_session = cayw.call('work.session.create', {{
  workspace = '{alpha_id}',
  label = 'Alpha Worker Session'
}})
local w = cayw.call('work.create', {{
  work_id = 'work-fullchain-alpha',
  title = '跨工作区全链路任务',
  goal = '在 project-alpha 自动创建会话并执行任务',
  acceptance_criteria = {{ {{criterion_id = 'c1', description = '任务完成'}} }},
  session_refs = {{ {{session_id = created_session.id, workspace = '{alpha_id}'}} }}
}})
local dispatched = cayw.call('work.dispatch', {{
  work_id = w.work_id,
  workspace = '{alpha_id}',
  session_id = created_session.id,
  text = 'EXTERNAL_WORKER_TASK: 请返回子工作区执行结果',
  dispatch_key = 'dk-fullchain-alpha-1'
}})
return {{work_id = w.work_id, alpha_session = created_session.id, task_id = dispatched.task_id}}
"#
                    );
                    let args = json!({
                        "action": "exec",
                        "script": script,
                        "yield_time_ms": 10000,
                        "max_output_tokens": 8192
                    });
                    json!({
                        "id": resp_id,
                        "model": body["model"],
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "tool_calls": [{
                                    "index": 0,
                                    "id": format!("call-{resp_id}"),
                                    "type": "function",
                                    "function": {
                                        "name": "execute_code",
                                        "arguments": args.to_string()
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }],
                        "usage": {"prompt_tokens": 32, "completion_tokens": 40, "total_tokens": 72}
                    })
                } else if last_user_text.contains("EXTERNAL_WORKER_TASK") {
                    json!({
                        "id": resp_id,
                        "model": body["model"],
                        "choices": [{
                            "index": 0,
                            "delta": {"role": "assistant", "content": "ALPHA_EXTERNAL_DONE"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20}
                    })
                } else {
                    let reply = format!(
                        "主会话已完成处理（{}）",
                        "全链路多进程上下文填充响应".repeat(12)
                    );
                    json!({
                        "id": resp_id,
                        "model": body["model"],
                        "choices": [{
                            "index": 0,
                            "delta": {"role": "assistant", "content": reply},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 24, "completion_tokens": 28, "total_tokens": 52}
                    })
                };

                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let http = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = socket.write_all(http.as_bytes()).await;
            });
        }
    });

    let watchdog = Duration::from_secs(30);
    let spawn_serve = || async {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cayw"))
            .env_clear()
            .env("HOME", &home)
            .env("CAYW_HOME", &state)
            .current_dir(&project_alpha)
            .arg("serve")
            .arg("--config")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(watchdog, lines.next_line())
            .await
            .expect("cayw serve startup timed out")
            .unwrap()
            .expect("cayw serve exited without ready JSON");
        let ready: Value = serde_json::from_str(&line).unwrap();
        let token = std::fs::read_to_string(ready["token_file"].as_str().unwrap()).unwrap();
        let client = cayw_ipc::Client::connect(
            ready["listening"].as_str().unwrap().parse().unwrap(),
            token.trim(),
        )
        .await
        .unwrap();
        (child, client)
    };

    let ipc_call = |client: &cayw_ipc::Client, method: &'static str, params: Value| {
        let client = client.clone();
        async move {
            tokio::time::timeout(watchdog, client.request(method, params))
                .await
                .unwrap_or_else(|_| panic!("IPC {method} timed out"))
                .unwrap_or_else(|err| panic!("IPC {method} failed: {err:?}"))
        }
    };

    let ws_call = |client: &cayw_ipc::Client, ws_id: &str, method: &'static str, params: Value| {
        let ws_id = ws_id.to_owned();
        ipc_call(
            client,
            "client.workspace.call",
            json!({"workspace_id": ws_id, "rpc": {"method": method, "params": params}}),
        )
    };

    let (mut server1, client1) = spawn_serve().await;

    // Verify custom `main_session_dir` from `--config` was opened and marked `main_session: true`!
    let listed = ipc_call(&client1, "settings.workspaces.list", json!({})).await;
    let entries = listed["workspaces"].as_array().unwrap();
    let main_entry = entries
        .iter()
        .find(|e| e["main_session"] == true)
        .expect("custom main_session_dir from --config must be opened with main_session=true");
    let main_ws_id = main_entry["id"].as_str().unwrap().to_owned();
    assert_eq!(
        Path::new(main_entry["path"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        custom_main_dir.canonicalize().unwrap()
    );
    let alpha_entry = entries
        .iter()
        .find(|e| e["main_session"] == false)
        .expect("project-alpha must be listed");
    let alpha_ws_id = alpha_entry["id"].as_str().unwrap().to_owned();
    *project_alpha_id_slot.lock().unwrap() = alpha_ws_id.clone();

    // Configure real OpenAI SSE model profile (`context_window: 2300`) and select it for both workspaces.
    ipc_call(
        &client1,
        "settings.models.put",
        json!({
            "name": "e2e-openai",
            "model": {
                "service": "openai",
                "api": "chat_completions",
                "base_url": base_url,
                "id": "e2e-fullchain-model",
                "api_version": "v1",
                "context_window": 2300,
                "max_output_tokens": 512,
                "api_key": "SYNTHETIC_E2E_KEY"
            }
        }),
    )
    .await;
    ipc_call(
        &client1,
        "settings.models.use",
        json!({"name": "e2e-openai"}),
    )
    .await;
    ws_call(
        &client1,
        &main_ws_id,
        "settings.models.use",
        json!({"name": "e2e-openai"}),
    )
    .await;

    // Verify `proactive.status` + `work.proactive.set` RPCs on the main session workspace.
    let initial_proactive = ws_call(&client1, &main_ws_id, "proactive.status", json!({})).await;
    assert_eq!(initial_proactive["mode"], "off");
    let updated_proactive = ws_call(
        &client1,
        &main_ws_id,
        "work.proactive.set",
        json!({"mode": "auto"}),
    )
    .await;
    assert_eq!(updated_proactive["mode"], "auto");

    // Create main session and execute Turn 1: cross-workspace Luau `work.session.create` + `work.create` + `work.dispatch` AND `MEM_FACT_ONE`!
    let main_session = ws_call(
        &client1,
        &main_ws_id,
        "session.create",
        json!({"label": "Main FullChain"}),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    ws_call(
        &client1,
        &main_ws_id,
        "session.prompt",
        json!({
            "session_id": main_session,
            "text": format!("DISPATCH_CROSS_WORKSPACE 并记住 MEM_FACT_ONE {}", "跨工作区派发输入填充".repeat(14))
        }),
    )
    .await;

    // Wait for `project-alpha` external worker task to complete automatically via `ClientWorkRouter`,
    // AND for background `memory_writer` (`finish_reason: "length"`) to write `fullchain-fact-one`!
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let works = ws_call(&client1, &main_ws_id, "work.list", json!({})).await;
        let work_arr = works.as_array().unwrap();
        let mem_status = ws_call(&client1, &main_ws_id, "memory.status", json!({})).await;
        let alpha_sessions = ws_call(&client1, &alpha_ws_id, "session.list", json!({})).await;
        let alpha_done = alpha_sessions
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["label"] == "Alpha Worker Session");
        let alpha_msg_done = if let Some(s) = alpha_sessions
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["label"] == "Alpha Worker Session")
        {
            let view = ws_call(
                &client1,
                &alpha_ws_id,
                "session.get",
                json!({"session_id": s["id"]}),
            )
            .await;
            view["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m.to_string().contains("ALPHA_EXTERNAL_DONE"))
        } else {
            false
        };

        if !work_arr.is_empty()
            && alpha_done
            && alpha_msg_done
            && mem_status["entries"] == 1
            && mem_status["visible"] == 1
            && mem_status["user_set"] == 0
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for cross-workspace worker + stated memory: works={works}, mem={mem_status}, alpha_sessions={alpha_sessions}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    // Drive additional turns to push Turn 1's multi-message Luau tool turn into eviction + `StopReason::Length` compression/summary,
    // then verify `session.maintenance.status` AND another turn with active compression!
    for turn in 2..=6 {
        ws_call(
            &client1,
            &main_ws_id,
            "session.prompt",
            json!({
                "session_id": main_session,
                "text": format!("第{turn}轮推进：{}", "主会话上下文压缩与多消息工具轮驱逐验证".repeat(16))
            }),
        )
        .await;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let maint = ws_call(
            &client1,
            &main_ws_id,
            "session.maintenance.status",
            json!({"session_id": main_session}),
        )
        .await;
        if maint["compressions"].as_u64().unwrap_or(0) > 0
            || maint["summaries"].as_u64().unwrap_or(0) > 0
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected session.maintenance.status to report published Length summary/compression: {maint}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    // Forget `fullchain-fact-one`'s source range via `memory.forget`, verify `visible == 0` while `active == 1` on disk,
    // then add `MEM_FACT_TWO` on a subsequent turn (which also exercises OpenAiProvider with active Compression/Summary!).
    let entries_before_forget = ws_call(&client1, &main_ws_id, "memory.entries", json!({})).await;
    let fact_one_log_id = entries_before_forget.as_array().unwrap()[0]["source_log_ids"]
        .as_array()
        .unwrap()[0]
        .as_str()
        .unwrap();
    let (_, cursor_str) = fact_one_log_id.rsplit_once(':').unwrap();
    let fact_one_cursor: u64 = cursor_str.parse().unwrap();
    ws_call(
        &client1,
        &main_ws_id,
        "memory.forget",
        json!({
            "session_id": main_session,
            "start_cursor": fact_one_cursor,
            "end_cursor": fact_one_cursor,
            "reason": "全链路测试遗忘第一条记忆"
        }),
    )
    .await;
    let status_after_forget = ws_call(&client1, &main_ws_id, "memory.status", json!({})).await;
    assert_eq!(status_after_forget["entries"], 1);
    assert_eq!(status_after_forget["active"], 1);
    assert_eq!(status_after_forget["visible"], 0);
    assert_eq!(
        ws_call(&client1, &main_ws_id, "memory.entries", json!({}))
            .await
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // Add second memory fact (`MEM_FACT_TWO`) while Compression/Summary is active in the session!
    ws_call(
        &client1,
        &main_ws_id,
        "session.prompt",
        json!({
            "session_id": main_session,
            "text": "继续对话并记住 MEM_FACT_TWO"
        }),
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let st = ws_call(&client1, &main_ws_id, "memory.status", json!({})).await;
        if st["entries"] == 2 && st["visible"] == 1 {
            assert_eq!(st["active"], 2);
            assert_eq!(st["user_set"], 0);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected MEM_FACT_TWO to be committed: {st}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    // Restart `cayw serve --config <path>` and verify durable persistence of:
    // 1. `proactive.status.mode == "auto"`
    // 2. `memory.status`: `entries == 2, active == 2, visible == 1, user_set == 0` (never reclassified as UserSet!)
    client1.close().await;
    server1.kill().await.unwrap();
    let _ = server1.wait().await;
    let _ = std::fs::remove_file(state.join("client.json"));

    let (mut server2, client2) = spawn_serve().await;
    let reopened_proactive = ws_call(&client2, &main_ws_id, "proactive.status", json!({})).await;
    assert_eq!(
        reopened_proactive["mode"], "auto",
        "proactive mode must persist across Client restart"
    );
    let reopened_mem = ws_call(&client2, &main_ws_id, "memory.status", json!({})).await;
    assert_eq!(reopened_mem["entries"], 2);
    assert_eq!(reopened_mem["active"], 2);
    assert_eq!(reopened_mem["visible"], 1);
    assert_eq!(
        reopened_mem["user_set"], 0,
        "stated memories must remain Stated (user_set == 0) across restart: {reopened_mem}"
    );

    client2.close().await;
    server2.kill().await.unwrap();
    let _ = server2.wait().await;
    sse_server.abort();
}
