//! Confined read-only traversal shared by glob and full-file grep.
use crate::{CapabilityError, Context, Workspace, failure, filesystem, io_error, json_size, parse};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use globset::GlobBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_GLOB_BYTES: usize = 4096;
const MAX_PATTERN_BYTES: usize = 16384;
const MAX_IGNORE_BYTES: usize = 64 * 1024;
const MAX_IGNORE_RULES: usize = 1024;
const REGEX_COMPILED_BYTES: usize = 1024 * 1024;
const REGEX_CACHE_BYTES: usize = 256 * 1024;
const REGEX_NEST: u32 = 64;
// Repeated regex search can rescan the suffix. Charge every suffix before search,
// multiplied by pattern bytes, including failed searches. The compiled program
// and its cache have separate hard bounds above.
const MAX_REGEX_WORK: usize = 256 * 1024 * 1024;

fn yes() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    path: String,
    pattern: String,
    #[serde(default = "yes")]
    respect_gitignore: bool,
    max_results: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    path: String,
    pattern: String,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    multiline: bool,
    #[serde(default)]
    dot_matches_new_line: bool,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default = "yes")]
    respect_gitignore: bool,
    max_matches: Option<usize>,
    #[serde(default)]
    context_lines: usize,
}

pub(crate) fn bound(
    value: Option<usize>,
    default: usize,
    ceiling: usize,
) -> Result<usize, CapabilityError> {
    let value = value.unwrap_or(default.min(ceiling));
    if value == 0 || value > ceiling {
        return Err(failure("invalid read result or entry bound"));
    }
    Ok(value)
}

// Charge JSON as each result is added, before an unbounded result can accumulate.
pub(crate) struct OutputBudget {
    used: usize,
    limit: usize,
}
impl OutputBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self { used: 0, limit }
    }
    pub(crate) fn add(&mut self, value: &Value) -> Result<(), CapabilityError> {
        let size = json_size(value, self.limit.saturating_sub(self.used))?;
        self.used = self.used.saturating_add(size).saturating_add(1);
        if self.used > self.limit {
            return Err(failure("read output byte limit exceeded"));
        }
        Ok(())
    }
}

enum Start {
    Directory(Dir),
    File(Dir, String),
}
struct IgnoreLayer {
    base: String,
    matcher: Gitignore,
}
struct Scanner<'a> {
    workspace: &'a Workspace,
    context: &'a Context,
    respect: bool,
    max_entries: usize,
    entries: usize,
    files: usize,
    bytes: usize,
    ignore_bytes: usize,
    ignore_rules: usize,
    ignored: usize,
    protected: usize,
    layers: Vec<IgnoreLayer>,
    skipped: Vec<Value>,
    output: OutputBudget,
}
impl<'a> Scanner<'a> {
    fn new(
        workspace: &'a Workspace,
        context: &'a Context,
        respect: bool,
    ) -> Result<Self, CapabilityError> {
        Ok(Self {
            workspace,
            context,
            respect,
            max_entries: workspace.limits.max_tree_entries,
            entries: 1,
            files: 0,
            bytes: 0,
            ignore_bytes: 0,
            ignore_rules: 0,
            ignored: 0,
            protected: 0,
            layers: vec![],
            skipped: vec![],
            output: OutputBudget::new(workspace.limits.max_output_bytes),
        })
    }
    fn read(&mut self, dir: &Dir, name: &str, ignore: bool) -> Result<Vec<u8>, CapabilityError> {
        self.context.check()?;
        let remaining = self
            .workspace
            .limits
            .max_batch_bytes
            .saturating_sub(self.bytes);
        let mut limit = self.workspace.limits.max_file_bytes.min(remaining);
        if ignore {
            limit = limit.min(MAX_IGNORE_BYTES.saturating_sub(self.ignore_bytes));
        }
        let bytes = filesystem::read(dir, name, limit, self.context)?;
        self.bytes += bytes.len();
        if ignore {
            self.ignore_bytes += bytes.len();
        }
        self.context.check()?;
        Ok(bytes)
    }
    fn ignores(&mut self, dir: &Dir, base: &str) -> Result<(), CapabilityError> {
        if !self.respect {
            return Ok(());
        }
        self.context.check()?;
        if let Some(meta) = filesystem::metadata(dir, ".gitignore")? {
            if !meta.is_file() {
                return Err(failure(".gitignore must be a regular no-follow file"));
            }
            let bytes = self.read(dir, ".gitignore", true)?;
            let text =
                std::str::from_utf8(&bytes).map_err(|_| failure(".gitignore must be UTF-8"))?;
            if text.contains('\0') {
                return Err(failure(".gitignore must not contain binary data"));
            }
            let mut builder = GitignoreBuilder::new(".");
            // Never use add(path): rules come exclusively from the held handle.
            for line in text.lines() {
                self.context.check()?;
                self.ignore_rules += 1;
                if self.ignore_rules > MAX_IGNORE_RULES.min(self.workspace.limits.max_tree_entries)
                    || line.len() > MAX_GLOB_BYTES
                {
                    return Err(failure("gitignore rule complexity limit exceeded"));
                }
                builder
                    .add_line(None, line)
                    .map_err(|error| failure(format!("gitignore rule: {error}")))?;
            }
            let matcher = builder
                .build()
                .map_err(|error| failure(format!("gitignore compilation: {error}")))?;
            self.context.check()?;
            self.layers.push(IgnoreLayer {
                base: base.into(),
                matcher,
            });
        }
        Ok(())
    }
    fn ignored(&self, path: &str, directory: bool) -> bool {
        for layer in self.layers.iter().rev() {
            let relative = if layer.base == "." {
                path
            } else {
                let Some(relative) = path
                    .strip_prefix(&layer.base)
                    .and_then(|p| p.strip_prefix('/'))
                else {
                    continue;
                };
                relative
            };
            // Every ancestor was already checked before descent. A whitelist of
            // an ancestor directory must not override an older rule matching
            // the current file (for example *.tmp with a deeper !kept/).
            let matched = layer.matcher.matched(relative, directory);
            if matched.is_whitelist() {
                return false;
            }
            if matched.is_ignore() {
                return true;
            }
        }
        false
    }
    fn start(&mut self, path: &str, allow_file: bool) -> Result<Option<Start>, CapabilityError> {
        let parts = filesystem::components(path, true)?;
        if parts.len() > self.workspace.limits.max_depth {
            return Err(failure("scan depth limit exceeded"));
        }
        let mut dir = self.workspace.root.try_clone().map_err(io_error)?;
        self.ignores(&dir, ".")?;
        let mut base = String::new();
        for (index, part) in parts.iter().enumerate() {
            self.context.check()?;
            if self.entries >= self.max_entries {
                return Err(failure("scan entry limit exceeded"));
            }
            self.entries += 1;
            let meta = dir.symlink_metadata(part).map_err(io_error)?;
            if !(meta.is_dir() || allow_file && index + 1 == parts.len() && meta.is_file()) {
                return Err(failure(
                    "scan root must be a no-follow directory or permitted regular file",
                ));
            }
            if !base.is_empty() {
                base.push('/');
            }
            base.push_str(part);
            if self.ignored(&base, meta.is_dir()) {
                self.ignored += 1;
                return Ok(None);
            }
            if meta.is_file() {
                filesystem::regular(&dir, part)?;
                return Ok(Some(Start::File(dir, (*part).into())));
            }
            dir = dir.open_dir_nofollow(part).map_err(io_error)?;
            self.ignores(&dir, &base)?;
        }
        Ok(Some(Start::Directory(dir)))
    }
    fn run(
        &mut self,
        path: &str,
        allow_file: bool,
        visitor: &mut impl Visitor,
    ) -> Result<bool, CapabilityError> {
        match self.start(path, allow_file)? {
            Some(Start::Directory(dir)) => self.walk(
                &dir,
                path,
                filesystem::components(path, true)?.len(),
                visitor,
            ),
            Some(Start::File(dir, name)) => {
                self.files += 1;
                visitor.visit(self, &dir, &name, path)
            }
            None => Ok(true),
        }
    }
    fn skip(&mut self, path: &str, reason: &str) -> Result<(), CapabilityError> {
        let item = json!({"path":path,"reason":reason});
        self.output.add(&item)?;
        self.skipped.push(item);
        Ok(())
    }
    fn walk(
        &mut self,
        dir: &Dir,
        path: &str,
        depth: usize,
        visitor: &mut impl Visitor,
    ) -> Result<bool, CapabilityError> {
        self.context.check()?;
        let mut names = vec![];
        // Bound discovery before sorting, including ignored and protected names.
        for entry in dir.entries().map_err(io_error)? {
            self.context.check()?;
            if self.entries >= self.max_entries {
                return Err(failure("scan entry limit exceeded"));
            }
            self.entries += 1;
            let name = entry
                .map_err(io_error)?
                .file_name()
                .into_string()
                .map_err(|_| failure("scan entry name is not UTF-8"))?;
            if filesystem::reserved(&name) {
                self.protected += 1;
                continue;
            }
            filesystem::components(&name, false)?;
            names.push(name);
        }
        names.sort();
        for name in names {
            self.context.check()?;
            let full = if path == "." {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            filesystem::components(&full, false)?;
            let meta = dir.symlink_metadata(&name).map_err(io_error)?;
            if self.ignored(&full, meta.is_dir()) {
                self.ignored += 1;
                continue;
            }
            if meta.is_dir() {
                if depth >= self.workspace.limits.max_depth {
                    return Err(failure("scan depth limit exceeded"));
                }
                let child = dir.open_dir_nofollow(&name).map_err(io_error)?;
                let count = self.layers.len();
                self.ignores(&child, &full)?;
                if !self.walk(&child, &full, depth + 1, visitor)? {
                    return Ok(false);
                }
                self.layers.truncate(count);
            } else if meta.is_file() {
                // Verify the opened entry itself is regular even in glob-only mode.
                let file = filesystem::regular(dir, &name)?;
                self.context.check()?;
                drop(file);
                self.files += 1;
                if !visitor.visit(self, dir, &name, &full)? {
                    return Ok(false);
                }
            } else {
                self.skip(
                    &full,
                    if meta.file_type().is_symlink() {
                        "symlink"
                    } else {
                        "special_file"
                    },
                )?;
            }
        }
        Ok(true)
    }
    fn stats(&self) -> Value {
        json!({"entries_examined":self.entries,"files_examined":self.files,"bytes_read":self.bytes,"ignore_bytes_read":self.ignore_bytes,"ignored_entries":self.ignored,"protected_entries_omitted":self.protected})
    }
}
trait Visitor {
    fn visit(
        &mut self,
        scan: &mut Scanner<'_>,
        dir: &Dir,
        name: &str,
        path: &str,
    ) -> Result<bool, CapabilityError>;
}
struct GlobVisitor {
    matcher: globset::GlobMatcher,
    prefix: String,
    max: usize,
    results: Vec<Value>,
}
impl Visitor for GlobVisitor {
    fn visit(
        &mut self,
        scan: &mut Scanner<'_>,
        _: &Dir,
        _: &str,
        path: &str,
    ) -> Result<bool, CapabilityError> {
        let relative = if self.prefix == "." {
            path
        } else {
            path.strip_prefix(&self.prefix)
                .and_then(|p| p.strip_prefix('/'))
                .ok_or_else(|| failure("internal scan prefix mismatch"))?
        };
        if self.matcher.is_match(relative) {
            if self.results.len() == self.max {
                return Ok(false);
            }
            let value = json!({"path":path,"kind":"file"});
            scan.output.add(&value)?;
            self.results.push(value);
        }
        Ok(true)
    }
}

pub(crate) struct Lines {
    starts: Vec<usize>,
}
impl Lines {
    pub(crate) fn new(text: &str, context: &Context) -> Result<Self, CapabilityError> {
        let mut starts = vec![0];
        for (chunk_index, chunk) in text.as_bytes().chunks(8192).enumerate() {
            context.check()?;
            for (at, byte) in chunk.iter().enumerate() {
                if *byte == b'\n' {
                    starts.push(chunk_index * 8192 + at + 1);
                }
            }
        }
        Ok(Self { starts })
    }
    pub(crate) fn line(&self, byte: usize) -> usize {
        self.starts.partition_point(|start| *start <= byte)
    }
    pub(crate) fn last_line(&self, start: usize, end: usize) -> usize {
        self.line(if end > start { end - 1 } else { end })
    }
    fn context(&self, text: &str, start: usize, end: usize, count: usize) -> Value {
        let first = self.line(start).saturating_sub(count).max(1);
        let last = (self.last_line(start, end) + count).min(self.starts.len());
        let byte_start = self.starts[first - 1];
        let byte_end = self.starts.get(last).copied().unwrap_or(text.len());
        // A trailing newline marks EOF, not another context line.
        let last = if last > first && byte_end == text.len() && self.starts[last - 1] == byte_end {
            last - 1
        } else {
            last
        };
        json!({"start_line":first,"end_line":last,"text":&text[byte_start..byte_end]})
    }
}
struct GrepVisitor {
    regex: Regex,
    max: usize,
    context_lines: usize,
    matches: Vec<Value>,
    work: usize,
}
impl Visitor for GrepVisitor {
    fn visit(
        &mut self,
        scan: &mut Scanner<'_>,
        dir: &Dir,
        name: &str,
        path: &str,
    ) -> Result<bool, CapabilityError> {
        let bytes = scan.read(dir, name, false)?;
        if bytes.contains(&0) {
            scan.skip(path, "binary")?;
            return Ok(true);
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            scan.skip(path, "non_utf8")?;
            return Ok(true);
        };
        let sha256 = filesystem::hash(&bytes);
        let lines = Lines::new(text, scan.context)?;
        let mut at = 0;
        let mut previous_end = None;
        while at <= text.len() {
            scan.context.check()?;
            // Enforce a deterministic finite amount of suffix searching, including
            // pathological patterns whose individual search has no interruption API.
            let cost = (text.len() - at + 1).saturating_mul(self.regex.as_str().len().max(1));
            self.work = self.work.saturating_add(cost);
            if self.work > MAX_REGEX_WORK {
                return Err(failure("regex search work limit exceeded"));
            }
            let Some(found) = self.regex.find_at(text, at) else {
                break;
            };
            scan.context.check()?;
            if found.is_empty() && previous_end == Some(found.end()) {
                let Some(next) = text[found.end()..].chars().next() else {
                    break;
                };
                at = found.end() + next.len_utf8();
                continue;
            }
            if self.matches.len() == self.max {
                return Ok(false);
            }
            let value = json!({"path":path,"sha256":sha256,"byte_start":found.start(),"byte_end":found.end(),"start_line":lines.line(found.start()),"end_line":lines.last_line(found.start(),found.end()),"text":found.as_str(),"context":lines.context(text,found.start(),found.end(),self.context_lines)});
            scan.output.add(&value)?;
            self.matches.push(value);
            previous_end = Some(found.end());
            if found.start() == found.end() {
                let Some(next) = text[found.end()..].chars().next() else {
                    break;
                };
                at = found.end() + next.len_utf8();
            } else {
                at = found.end();
            }
        }
        Ok(true)
    }
}

pub(crate) fn execute(
    workspace: &Workspace,
    name: &str,
    args: &Value,
    context: &Context,
) -> Result<Value, CapabilityError> {
    context.check()?;
    match name {
        "fs.glob" => {
            let args: GlobArgs = parse(args)?;
            if args.pattern.is_empty()
                || args.pattern.len() > MAX_GLOB_BYTES
                || args.pattern.starts_with('/')
                || args.pattern.contains('\0')
                || args.pattern.split('/').any(|p| p == ".." || p == ".")
            {
                return Err(failure("expected a bounded relative glob pattern"));
            }
            let matcher = GlobBuilder::new(&args.pattern)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .map_err(|error| failure(format!("glob pattern: {error}")))?
                .compile_matcher();
            context.check()?;
            let max = bound(args.max_results, 1000, workspace.limits.max_tree_entries)?;
            let mut scan = Scanner::new(workspace, context, args.respect_gitignore)?;
            let mut visitor = GlobVisitor {
                matcher,
                prefix: args.path.clone(),
                max,
                results: vec![],
            };
            let complete = scan.run(&args.path, false, &mut visitor)?;
            context.check()?;
            let paths: Vec<&str> = visitor
                .results
                .iter()
                .filter_map(|v| v["path"].as_str())
                .collect();
            Ok(
                json!({"path":args.path,"pattern":args.pattern,"paths":paths,"skipped":scan.skipped,"scan_complete":complete,"truncated":!complete,"protected_entries_omitted":true,"respect_gitignore":args.respect_gitignore,"scan":scan.stats()}),
            )
        }
        "fs.grep" => {
            let args: GrepArgs = parse(args)?;
            if args.pattern.len() > MAX_PATTERN_BYTES || args.context_lines > 64 {
                return Err(failure("regex pattern or context limit exceeded"));
            }
            let pattern = if args.literal {
                regex::escape(&args.pattern)
            } else {
                args.pattern.clone()
            };
            let regex = RegexBuilder::new(&pattern)
                .case_insensitive(args.case_insensitive)
                .multi_line(args.multiline)
                .dot_matches_new_line(args.dot_matches_new_line)
                .size_limit(REGEX_COMPILED_BYTES)
                .dfa_size_limit(REGEX_CACHE_BYTES)
                .nest_limit(REGEX_NEST)
                .build()
                .map_err(|error| failure(format!("regex compilation: {error}")))?;
            context.check()?;
            let max = bound(args.max_matches, 100, workspace.limits.max_tree_entries)?;
            let mut scan = Scanner::new(workspace, context, args.respect_gitignore)?;
            let mut visitor = GrepVisitor {
                regex,
                max,
                context_lines: args.context_lines,
                matches: vec![],
                work: 0,
            };
            let complete = scan.run(&args.path, true, &mut visitor)?;
            context.check()?;
            Ok(
                json!({"path":args.path,"matches":visitor.matches,"skipped":scan.skipped,"scan_complete":complete,"truncated":!complete,"protected_entries_omitted":true,"respect_gitignore":args.respect_gitignore,"scan":scan.stats()}),
            )
        }
        _ => Err(failure("unknown search capability")),
    }
}
