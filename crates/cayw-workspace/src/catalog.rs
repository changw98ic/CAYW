use cayw_ai::ToolDefinition;
use serde_json::{Value, json};

fn definition(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: object(properties, required),
    }
}
fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn path() -> Value {
    json!({"type":"string","minLength":1,"maxLength":4096})
}
fn hash() -> Value {
    json!({"type":"string","pattern":"^[0-9a-fA-F]{64}$"})
}
fn batch() -> Value {
    json!({"type":"string","pattern":"^b-[0-9a-f-]{4,94}$"})
}
fn bound() -> Value {
    json!({"type":"integer","minimum":1,"maximum":100000})
}
fn language() -> Value {
    json!({"type":"string","minLength":1,"maxLength":64})
}

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    let expected = json!({"oneOf":[
        object(json!({"kind":{"const":"absent"}}), &["kind"]),
        object(json!({"kind":{"const":"sha256"},"sha256":hash()}), &["kind","sha256"])
    ]});
    let write = object(
        json!({"op":{"const":"write"},"path":path(),"expected":expected,"content":{"type":"string"},"encoding":{"enum":["utf8","base64"]},"language":language()}),
        &["op", "path", "expected", "content", "encoding", "language"],
    );
    let patch = object(
        json!({"op":{"const":"patch"},"path":path(),"expected_sha256":hash(),"old":{"type":"string","minLength":1},"new":{"type":"string"},"language":language()}),
        &["op", "path", "expected_sha256", "old", "new", "language"],
    );
    let remove = object(
        json!({"op":{"const":"remove"},"path":path(),"expected_sha256":hash()}),
        &["op", "path", "expected_sha256"],
    );
    vec![
        definition(
            "fs.glob",
            "Find bounded regular files by a portable glob relative to path. Uses workspace and nested .gitignore rules by default, without ambient config or following links. Results are workspace-relative; explicit truncation only limits matches.",
            json!({"path":path(),"pattern":{"type":"string","minLength":1,"maxLength":4096},"max_results":bound(),"respect_gitignore":{"type":"boolean"}}),
            &["path", "pattern"],
        ),
        definition(
            "fs.grep",
            "Search bounded complete UTF-8 regular files with a bounded Rust regex, or explicit literal=true. Reports source SHA-256, half-open byte ranges and inclusive 1-based lines. Binary and non-UTF8 files are explicitly skipped; scan bounds fail rather than imply completeness.",
            json!({"path":path(),"pattern":{"type":"string","maxLength":16384},"literal":{"type":"boolean"},"multiline":{"type":"boolean"},"dot_matches_new_line":{"type":"boolean"},"case_insensitive":{"type":"boolean"},"respect_gitignore":{"type":"boolean"},"max_matches":bound(),"context_lines":{"type":"integer","minimum":0,"maximum":64}}),
            &["path", "pattern"],
        ),
        definition(
            "code.outline",
            "Parse one bounded Rust file into a local syntax outline with source SHA-256 and symbol ranges. Includes inline modules, impl and trait members. No macro expansion, resolution or external module reads; unsupported languages and malformed Rust fail.",
            json!({"path":path(),"language":{"const":"rust"},"max_symbols":bound()}),
            &["path", "language"],
        ),
        definition(
            "code.diff_hunks",
            "Validate unified text diff headers and body counts, returning paths and exact old/new line spans. Reads only the supplied diff, with no Git process or filesystem access. Checks all input even when max_hunks truncates the returned list.",
            json!({"diff":{"type":"string"},"max_hunks":bound()}),
            &["diff"],
        ),
        definition(
            "fs.mkdir",
            "Create exactly one absent directory; no implicit parent creation.",
            json!({"path":path(),"expected_absent":{"const":true}}),
            &["path", "expected_absent"],
        ),
        definition(
            "fs.move",
            "Move a regular file after source SHA-256 and destination absence checks. Link/unlink has a visible two-path window.",
            json!({"source":path(),"destination":path(),"expected_sha256":hash(),"expected_destination_absent":{"const":true}}),
            &[
                "source",
                "destination",
                "expected_sha256",
                "expected_destination_absent",
            ],
        ),
        definition(
            "fs.remove",
            "Remove one exact file or empty directory. Recursive directory removal requires explicit recursive=true, expected_root_sha256 and max_entries; never follows links.",
            json!({"path":path(),"expected_sha256":hash(),"recursive":{"type":"boolean"},"expected_root_sha256":hash(),"max_entries":bound()}),
            &["path"],
        ),
        definition(
            "fs.tree",
            "Hash a bounded file tree; protected entries are omitted and links/special entries are rejected. Root is included in max_entries.",
            json!({"path":path(),"max_entries":bound()}),
            &["path", "max_entries"],
        ),
        definition(
            "fs.prepare_change_batch",
            "Prepare exact regular-file writes, unique text patches and removals. Persist backups and syntax-validated candidate bytes without changing user paths.",
            json!({"operations":{"type":"array","minItems":1,"maxItems":256,"items":{"oneOf":[write,patch,remove]}}}),
            &["operations"],
        ),
        definition(
            "fs.submit_change_batch",
            "Submit a prepared digest after rechecking every path under the shared workspace lock. Multi-path changes are recoverable but not atomically visible.",
            json!({"batch_id":batch(),"expected_batch_digest":hash()}),
            &["batch_id", "expected_batch_digest"],
        ),
        definition(
            "fs.batch_status",
            "Read persisted phase and current old/new/third-party evidence per path. Omit batch_id to list published journals.",
            json!({"batch_id":batch()}),
            &[],
        ),
        definition(
            "fs.recover_change_batch",
            "Explicitly commit or roll back an interrupted batch after checking every path. Third-party changes block all recovery writes.",
            json!({"batch_id":batch(),"expected_batch_digest":hash(),"action":{"enum":["commit","rollback"]}}),
            &["batch_id", "expected_batch_digest", "action"],
        ),
        definition(
            "archive.list",
            "List one bounded workspace ZIP or uncompressed TAR after checking all entry paths, kinds, duplicates and size/ratio limits. No extraction.",
            json!({"path":path(),"format":{"enum":["zip","tar"]}}),
            &["path", "format"],
        ),
        definition(
            "archive.read",
            "Read one entry from a validated bounded workspace ZIP or uncompressed TAR, marked utf8 or base64. No extraction or nested decompression.",
            json!({"path":path(),"format":{"enum":["zip","tar"]},"entry":path()}),
            &["path", "format", "entry"],
        ),
    ]
}
