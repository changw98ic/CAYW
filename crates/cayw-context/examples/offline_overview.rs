//! Offline demo: parent + child share one budget overview; unknown cost stays empty.

use cayw_context::*;

fn tokens(input: u64, output: u64, cache_read: u64) -> TokenTotals {
    let mut totals = TokenTotals {
        input,
        output,
        cache_read,
        cache_write: 0,
        reasoning: 0,
        total_tokens: 0,
    };
    totals.update_total();
    totals
}

fn main() {
    let root = SessionLedgerSnapshot {
        session_id: "sess-root".into(),
        budget_session_id: "sess-root".into(),
        parent_session_id: None,
        label: Some("Parent".into()),
        token_limit: 100_000,
        settled_tokens: 1_200,
        reserved_tokens: 0,
        remaining_tokens: 98_800,
        budget_unknown: false,
        cost_status: CostStatus::Unknown,
        cost_amount: None,
        cost_currency: None,
        requests: vec![RequestUsage {
            request_id: "req-1".into(),
            session_id: "sess-root".into(),
            task_id: "task-root".into(),
            budget_session_id: "sess-root".into(),
            parent_session_id: None,
            turn: 1,
            provider: "fixture".into(),
            model: "demo".into(),
            terminal: Some(RequestTerminal::Completed),
            usage_status: UsageReportStatus::Reported,
            usage: tokens(800, 300, 100),
            cost_status: CostStatus::Unknown,
            cost_amount: None,
            cost_currency: None,
        }],
    };
    let child = SessionLedgerSnapshot {
        session_id: "sess-child".into(),
        budget_session_id: "sess-root".into(),
        parent_session_id: Some("sess-root".into()),
        label: Some("Sub-agent".into()),
        token_limit: 100_000,
        settled_tokens: 450,
        reserved_tokens: 0,
        remaining_tokens: 98_350,
        budget_unknown: false,
        cost_status: CostStatus::Unknown,
        cost_amount: None,
        cost_currency: None,
        requests: vec![RequestUsage {
            request_id: "req-2".into(),
            session_id: "sess-child".into(),
            task_id: "task-child".into(),
            budget_session_id: "sess-root".into(),
            parent_session_id: Some("sess-root".into()),
            turn: 1,
            provider: "fixture".into(),
            model: "demo".into(),
            terminal: Some(RequestTerminal::Completed),
            usage_status: UsageReportStatus::Reported,
            usage: tokens(300, 100, 50),
            cost_status: CostStatus::Unknown,
            cost_amount: None,
            cost_currency: None,
        }],
    };

    let fleet = build_fleet_overview("sess-root", &[root.clone(), child.clone()]).unwrap();
    let cross = build_cross_session_overview(&[root, child]);

    println!("protocol={}", CONTEXT_PROTOCOL_VERSION);
    println!(
        "fleet members={} root={} total_tokens={} cost={:?}",
        fleet.member_count, fleet.root_session_id, fleet.tokens.total_tokens, fleet.cost.status
    );
    for member in &fleet.members {
        println!(
            "  member {} label={:?} tokens={} parent={:?}",
            member.session_id, member.label, member.tokens.total_tokens, member.parent_session_id
        );
    }
    println!(
        "cross sessions={} requests={} cache_hit={:?}",
        cross.session_count, cross.request_count, cross.cache.hit_rate
    );
}
