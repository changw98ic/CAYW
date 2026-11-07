use cayw_context::*;

fn tokens(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
) -> TokenTotals {
    let mut totals = TokenTotals {
        input,
        output,
        cache_read,
        cache_write,
        reasoning,
        total_tokens: 0,
    };
    totals.update_total();
    totals
}

fn request(
    id: &str,
    session: &str,
    parent: Option<&str>,
    budget: &str,
    turn: u64,
    usage: TokenTotals,
    cost: CostStatus,
) -> RequestUsage {
    RequestUsage {
        request_id: id.into(),
        session_id: session.into(),
        task_id: format!("task-{session}"),
        budget_session_id: budget.into(),
        parent_session_id: parent.map(str::to_owned),
        turn,
        provider: "fixture".into(),
        model: "fixture-model".into(),
        terminal: Some(RequestTerminal::Completed),
        usage_status: UsageReportStatus::Reported,
        usage,
        cost_status: cost,
        cost_amount: None,
        cost_currency: None,
    }
}

fn snapshot(
    session: &str,
    parent: Option<&str>,
    budget: &str,
    label: Option<&str>,
    requests: Vec<RequestUsage>,
) -> SessionLedgerSnapshot {
    let settled: u64 = requests.iter().map(|r| r.usage.total_tokens).sum();
    SessionLedgerSnapshot {
        session_id: session.into(),
        budget_session_id: budget.into(),
        parent_session_id: parent.map(str::to_owned),
        label: label.map(str::to_owned),
        token_limit: 1_000_000,
        settled_tokens: settled,
        reserved_tokens: 0,
        remaining_tokens: 1_000_000 - settled,
        budget_unknown: false,
        cost_status: CostStatus::Unknown,
        cost_amount: None,
        cost_currency: None,
        requests,
    }
}

#[test]
fn session_overview_folds_tokens_cache_and_composition() {
    let snap = snapshot(
        "root",
        None,
        "root",
        Some("Main"),
        vec![
            request(
                "r1",
                "root",
                None,
                "root",
                1,
                tokens(100, 40, 20, 5, 10),
                CostStatus::Unknown,
            ),
            request(
                "r2",
                "root",
                None,
                "root",
                2,
                tokens(80, 30, 40, 0, 0),
                CostStatus::Unknown,
            ),
        ],
    );
    let overview = build_session_overview(&snap);
    assert_eq!(overview.request_count, 2);
    assert_eq!(overview.turn_count, 2);
    assert_eq!(overview.tokens.input, 180);
    assert_eq!(overview.tokens.output, 70);
    assert_eq!(overview.tokens.cache_read, 60);
    assert_eq!(overview.tokens.cache_write, 5);
    assert_eq!(overview.tokens.reasoning, 10);
    assert_eq!(overview.tokens.total_tokens, 315);
    assert_eq!(overview.cost.status, CostStatus::Unknown);
    assert!(overview.cost.amount.is_none());
    assert!(overview.cost.currency.is_none());
    let rate = overview.cache.hit_rate.unwrap();
    assert!((rate - (60.0 / 240.0)).abs() < 1e-9);
    assert_eq!(overview.composition[0].kind, CompositionKind::Input);
    assert_eq!(overview.evolution.len(), 2);
    assert_eq!(overview.evolution[0].request_id, "r1");
}

#[test]
fn unknown_cost_never_fabricates_amount_across_sessions() {
    let a = snapshot(
        "a",
        None,
        "a",
        None,
        vec![request(
            "a1",
            "a",
            None,
            "a",
            1,
            tokens(10, 5, 0, 0, 0),
            CostStatus::Unknown,
        )],
    );
    let mut b = snapshot(
        "b",
        None,
        "b",
        None,
        vec![request(
            "b1",
            "b",
            None,
            "b",
            1,
            tokens(20, 5, 0, 0, 0),
            CostStatus::Reported,
        )],
    );
    b.cost_status = CostStatus::Reported;
    b.cost_amount = Some("0.01".into());
    b.cost_currency = Some("USD".into());
    b.requests[0].cost_status = CostStatus::Reported;
    b.requests[0].cost_amount = Some("0.01".into());
    b.requests[0].cost_currency = Some("USD".into());

    let cross = build_cross_session_overview(&[a, b]);
    assert_eq!(cross.session_count, 2);
    assert_eq!(cross.tokens.total_tokens, 40);
    assert_eq!(cross.cost.status, CostStatus::Unknown);
    assert!(cross.cost.amount.is_none());
}

#[test]
fn parent_child_fleet_shares_one_budget_screen() {
    let root = snapshot(
        "root",
        None,
        "root",
        Some("Parent"),
        vec![request(
            "p1",
            "root",
            None,
            "root",
            1,
            tokens(50, 20, 10, 0, 0),
            CostStatus::Unknown,
        )],
    );
    let child = snapshot(
        "child-1",
        Some("root"),
        "root",
        Some("Researcher"),
        vec![request(
            "c1",
            "child-1",
            Some("root"),
            "root",
            1,
            tokens(30, 15, 5, 0, 0),
            CostStatus::Unknown,
        )],
    );
    let other = snapshot(
        "other",
        None,
        "other",
        None,
        vec![request(
            "o1",
            "other",
            None,
            "other",
            1,
            tokens(999, 0, 0, 0, 0),
            CostStatus::Unknown,
        )],
    );

    let fleet = build_fleet_overview("root", &[root, child, other]).unwrap();
    assert_eq!(fleet.member_count, 2);
    assert_eq!(fleet.root_session_id, "root");
    assert_eq!(fleet.tokens.input, 80);
    assert_eq!(fleet.tokens.output, 35);
    assert_eq!(fleet.tokens.cache_read, 15);
    assert_eq!(fleet.tokens.total_tokens, 130);
    assert_eq!(fleet.cost.status, CostStatus::Unknown);
    assert!(fleet.members[0].is_root);
    assert_eq!(fleet.members[0].session_id, "root");
    assert_eq!(fleet.members[1].session_id, "child-1");
    assert_eq!(fleet.members[1].parent_session_id.as_deref(), Some("root"));
}

#[test]
fn cache_hit_rate_absent_when_no_input_or_cache() {
    assert!(cache_hit_rate(&TokenTotals::default()).is_none());
    let only_output = tokens(0, 10, 0, 0, 0);
    assert!(cache_hit_rate(&only_output).is_none());
}

#[test]
fn merge_cost_collapses_distinct_reported_amounts() {
    let a = CostView::reported("0.01", "USD");
    let b = CostView::reported("0.02", "USD");
    let merged = merge_cost([&a, &b]);
    assert_eq!(merged.status, CostStatus::Unknown);
    assert!(merged.amount.is_none());
}

#[test]
fn merge_cost_collapses_multiple_reported_amounts_even_with_same_amount() {
    let a = CostView::reported("0.01", "USD");
    let b = CostView::reported("0.01", "USD");
    let merged = merge_cost([&a, &b]);
    assert_eq!(merged.status, CostStatus::Unknown);
    assert!(merged.amount.is_none());
    assert!(merged.currency.is_none());
}

#[test]
fn merge_cost_retains_single_reported_amount() {
    let a = CostView::reported("0.01", "USD");
    let merged = merge_cost([&a]);
    assert_eq!(merged.status, CostStatus::Reported);
    assert_eq!(merged.amount.as_deref(), Some("0.01"));
    assert_eq!(merged.currency.as_deref(), Some("USD"));
}

#[test]
fn merge_cost_returns_unknown_for_empty_or_invalid() {
    let empty: Vec<CostView> = vec![];
    assert_eq!(merge_cost(empty.iter()).status, CostStatus::Unknown);

    let invalid = CostView {
        status: CostStatus::Reported,
        amount: Some("".into()),
        currency: Some("USD".into()),
    };
    assert_eq!(merge_cost([&invalid]).status, CostStatus::Unknown);
}

#[test]
fn cross_session_overview_collapses_two_sessions_with_same_reported_cost() {
    let mut a = snapshot(
        "a",
        None,
        "a",
        None,
        vec![request(
            "a1",
            "a",
            None,
            "a",
            1,
            tokens(10, 5, 0, 0, 0),
            CostStatus::Reported,
        )],
    );
    a.cost_status = CostStatus::Reported;
    a.cost_amount = Some("0.01".into());
    a.cost_currency = Some("USD".into());

    let mut b = snapshot(
        "b",
        None,
        "b",
        None,
        vec![request(
            "b1",
            "b",
            None,
            "b",
            1,
            tokens(20, 5, 0, 0, 0),
            CostStatus::Reported,
        )],
    );
    b.cost_status = CostStatus::Reported;
    b.cost_amount = Some("0.01".into());
    b.cost_currency = Some("USD".into());

    let cross = build_cross_session_overview(&[a.clone(), b]);
    assert_eq!(cross.session_count, 2);
    // Two independent sessions each with 0.01 USD must NOT be deduplicated into 0.01 USD.
    assert_eq!(cross.cost.status, CostStatus::Unknown);
    assert!(cross.cost.amount.is_none());

    // Single session retains reported cost
    let single = build_cross_session_overview(&[a]);
    assert_eq!(single.session_count, 1);
    assert_eq!(single.cost.status, CostStatus::Reported);
    assert_eq!(single.cost.amount.as_deref(), Some("0.01"));
    assert_eq!(single.cost.currency.as_deref(), Some("USD"));
}

#[test]
fn fleet_rejects_empty_budget_or_missing_members() {
    assert!(matches!(
        build_fleet_overview("", &[]),
        Err(ContextError::Invalid(_))
    ));
    assert!(matches!(
        build_fleet_overview("missing", &[]),
        Err(ContextError::Invalid(_))
    ));
}
