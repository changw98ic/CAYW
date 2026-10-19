use crate::{
    budget::{Budget, Result, Sink},
    error,
};
use serde_json::{Value, json};

#[derive(Clone, Copy)]
enum Kind {
    Equal,
    Delete,
    Insert,
}
struct Edit<'a> {
    kind: Kind,
    line: &'a str,
}

pub(crate) fn diff(args: &Value, budget: &Budget<'_>) -> Result<Value> {
    let before = args["before"].as_str().expect("validated before");
    let after = args["after"].as_str().expect("validated after");
    let context = args
        .get("context")
        .map(|v| {
            v.as_u64()
                .ok_or_else(|| error("arguments", "context must be an integer token"))
        })
        .transpose()?
        .unwrap_or(3) as usize;
    let old = lines(before, budget)?;
    let new = lines(after, budget)?;
    if before == after {
        return Ok(json!({"diff":""}));
    }
    let width = new.len() + 1;
    let cells = (old.len() + 1)
        .checked_mul(width)
        .filter(|&n| n <= budget.limits.max_diff_work)
        .ok_or_else(|| error("diff_work_limit", "diff matrix exceeds max_diff_work"))?;
    let mut remaining = budget.limits.max_diff_work - cells;
    let mut table = vec![0_u32; cells];
    for i in (0..old.len()).rev() {
        budget.checkpoint()?;
        for j in (0..new.len()).rev() {
            // Charge byte comparisons as well as matrix size, even for repeated long lines.
            let cost = old[i].len().min(new[j].len()) + 1;
            remaining = remaining.checked_sub(cost).ok_or_else(|| {
                error(
                    "diff_work_limit",
                    "diff byte comparisons exceed max_diff_work",
                )
            })?;
            table[i * width + j] = if old[i] == new[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut edits = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        budget.checkpoint()?;
        if i < old.len() && j < new.len() && old[i] == new[j] {
            edits.push(Edit {
                kind: Kind::Equal,
                line: old[i],
            });
            i += 1;
            j += 1;
        } else if i < old.len()
            && (j == new.len() || table[(i + 1) * width + j] >= table[i * width + j + 1])
        {
            edits.push(Edit {
                kind: Kind::Delete,
                line: old[i],
            });
            i += 1;
        } else {
            edits.push(Edit {
                kind: Kind::Insert,
                line: new[j],
            });
            j += 1;
        }
    }
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        if matches!(edit.kind, Kind::Equal) {
            continue;
        }
        let start = index.saturating_sub(context);
        let end = index.saturating_add(context + 1).min(edits.len());
        if let Some(last) = hunks.last_mut().filter(|last| start <= last.1) {
            last.1 = last.1.max(end);
        } else {
            hunks.push((start, end));
        }
    }
    let mut sink = Sink::new(budget.limits.output_bytes, budget.cancel);
    sink.push("--- before\n+++ after\n")?;
    let (mut old_position, mut new_position, mut position) = (0, 0, 0);
    for (start, end) in hunks {
        for edit in &edits[position..start] {
            old_position += usize::from(!matches!(edit.kind, Kind::Insert));
            new_position += usize::from(!matches!(edit.kind, Kind::Delete));
        }
        let old_count = edits[start..end]
            .iter()
            .filter(|e| !matches!(e.kind, Kind::Insert))
            .count();
        let new_count = edits[start..end]
            .iter()
            .filter(|e| !matches!(e.kind, Kind::Delete))
            .count();
        let old_start = old_position + usize::from(old_count > 0);
        let new_start = new_position + usize::from(new_count > 0);
        sink.push(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ))?;
        for edit in &edits[start..end] {
            sink.push(match edit.kind {
                Kind::Equal => " ",
                Kind::Delete => "-",
                Kind::Insert => "+",
            })?;
            sink.push(edit.line)?;
            if !edit.line.ends_with('\n') {
                sink.push("\n\\ No newline at end of file\n")?;
            }
            old_position += usize::from(!matches!(edit.kind, Kind::Insert));
            new_position += usize::from(!matches!(edit.kind, Kind::Delete));
        }
        position = end;
    }
    Ok(json!({"diff":sink.finish()?}))
}

fn lines<'a>(text: &'a str, budget: &Budget<'_>) -> Result<Vec<&'a str>> {
    let mut lines = Vec::new();
    for line in text.split_inclusive('\n') {
        budget.checkpoint()?;
        budget.rows(lines.len() + 1)?;
        if lines.len() >= budget.limits.max_nodes {
            return Err(error("node_limit", "diff lines exceed max_nodes"));
        }
        lines.push(line);
    }
    Ok(lines)
}
