//! Local parsed syntax outlines and pure unified-diff coordinates.
use crate::search::{Lines, OutputBudget, bound};
use crate::{CapabilityError, Context, Workspace, failure, filesystem, parse};
use proc_macro2::Span;
use serde::Deserialize;
use serde_json::{Value, json};
use syn::{ImplItem, Item, TraitItem, spanned::Spanned};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutlineArgs {
    path: String,
    language: String,
    max_symbols: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    diff: String,
    max_hunks: Option<usize>,
}

pub(crate) fn execute(
    workspace: &Workspace,
    name: &str,
    arguments: &Value,
    context: &Context,
) -> Result<Value, CapabilityError> {
    context.check()?;
    match name {
        "code.outline" => {
            let args: OutlineArgs = parse(arguments)?;
            if args.language != "rust" {
                return Err(failure("unsupported outline language; expected rust"));
            }
            let parts = filesystem::components(&args.path, false)?;
            if parts.len() > workspace.limits.max_depth {
                return Err(failure("outline path depth limit exceeded"));
            }
            let (dir, name) = filesystem::parent(&workspace.root, &parts)?;
            let bytes = filesystem::read(
                &dir,
                name,
                workspace
                    .limits
                    .max_file_bytes
                    .min(workspace.limits.max_batch_bytes),
                context,
            )?;
            let text =
                std::str::from_utf8(&bytes).map_err(|_| failure("Rust source must be UTF-8"))?;
            syntax_shape(text, workspace.limits.max_depth.min(64), context)?;
            context.check()?;
            let parsed =
                syn::parse_file(text).map_err(|error| failure(format!("Rust syntax: {error}")))?;
            context.check()?;
            let source_offset = usize::from(text.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
                + parsed.shebang.as_ref().map_or(0, String::len);
            let mut outline = Outline {
                text,
                source_offset,
                lines: Lines::new(text, context)?,
                symbols: vec![],
                max: bound(args.max_symbols, 1000, workspace.limits.max_tree_entries)?,
                total: 0,
                ceiling: workspace.limits.max_tree_entries,
                depth: workspace.limits.max_depth,
                context,
                output: OutputBudget::new(workspace.limits.max_output_bytes),
            };
            outline.items(&parsed.items, None, 0)?;
            context.check()?;
            Ok(
                json!({"path":args.path,"language":"rust","sha256":filesystem::hash(&bytes),"symbols":outline.symbols,"total_symbols":outline.total,"truncated":outline.total > outline.max,"syntax_only":true}),
            )
        }
        "code.diff_hunks" => {
            let args: DiffArgs = parse(arguments)?;
            if args.diff.len() > workspace.limits.max_batch_bytes || args.diff.contains('\0') {
                return Err(failure("diff input byte limit exceeded or binary input"));
            }
            let max = bound(args.max_hunks, 1000, workspace.limits.max_tree_entries)?;
            diff(&args.diff, max, workspace, context)
        }
        _ => Err(failure("unknown code capability")),
    }
}

// Conservative shape bound before entering synchronous recursive Rust parsing.
// Comments and literals are consumed iteratively; punctuation outside them bounds
// both nested and flat recursive expressions. It is a resource refusal, not syntax validation.
fn syntax_shape(text: &str, max_depth: usize, context: &Context) -> Result<(), CapabilityError> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut nesting = 0usize;
    let mut punctuation = 0usize;
    let mut sequence = 0usize;
    while at < bytes.len() {
        context.check()?;
        if bytes[at..].starts_with(b"//") {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        if bytes[at..].starts_with(b"/*") {
            at += 2;
            let mut depth = 1usize;
            while at < bytes.len() && depth > 0 {
                context.check()?;
                if bytes[at..].starts_with(b"/*") {
                    depth += 1;
                    at += 2;
                } else if bytes[at..].starts_with(b"*/") {
                    depth -= 1;
                    at += 2;
                } else {
                    at += 1;
                }
                if depth > max_depth {
                    return Err(failure("Rust syntax nesting limit exceeded"));
                }
            }
            continue;
        }
        let raw_start = if bytes[at..].starts_with(b"br") {
            Some(at + 2)
        } else if bytes[at] == b'r' {
            Some(at + 1)
        } else {
            None
        };
        if let Some(start) = raw_start {
            let mut quote = start;
            while bytes.get(quote) == Some(&b'#') {
                quote += 1;
            }
            if bytes.get(quote) == Some(&b'"') {
                let hashes = quote - start;
                at = quote + 1;
                while at < bytes.len() {
                    context.check()?;
                    if bytes[at] == b'"'
                        && bytes
                            .get(at + 1..at + 1 + hashes)
                            .is_some_and(|tail| tail.iter().all(|b| *b == b'#'))
                    {
                        at += 1 + hashes;
                        break;
                    }
                    at += 1;
                }
                continue;
            }
        }
        if bytes[at] == b'"' {
            at += 1;
            while at < bytes.len() {
                context.check()?;
                let byte = bytes[at];
                at += 1;
                if byte == b'\\' {
                    at = (at + 1).min(bytes.len());
                } else if byte == b'"' {
                    break;
                }
            }
            continue;
        }
        if bytes[at].is_ascii_punctuation() {
            punctuation += 1;
            sequence += 1;
            if matches!(bytes[at], b'(' | b'[' | b'{' | b'<') {
                nesting += 1;
            }
            if matches!(bytes[at], b')' | b']' | b'}' | b'>') {
                nesting = nesting.saturating_sub(1);
            }
            if bytes[at] == b';' {
                sequence = 0;
            }
            if nesting > max_depth || punctuation > 4096 || sequence > 512 {
                return Err(failure("Rust syntax complexity limit exceeded"));
            }
        }
        at += 1;
    }
    Ok(())
}

struct Outline<'a> {
    text: &'a str,
    source_offset: usize,
    lines: Lines,
    symbols: Vec<Value>,
    max: usize,
    total: usize,
    ceiling: usize,
    depth: usize,
    context: &'a Context,
    output: OutputBudget,
}
impl Outline<'_> {
    fn add(
        &mut self,
        name: String,
        kind: &str,
        span: Span,
        parent: Option<usize>,
    ) -> Result<usize, CapabilityError> {
        self.context.check()?;
        if self.total >= self.ceiling {
            return Err(failure("outline declaration limit exceeded"));
        }
        let range = span.byte_range();
        let range = range.start + self.source_offset..range.end + self.source_offset;
        if range.end > self.text.len()
            || range.start > range.end
            || !self.text.is_char_boundary(range.start)
            || !self.text.is_char_boundary(range.end)
        {
            return Err(failure("Rust parser returned invalid source range"));
        }
        let id = self.total;
        self.total += 1;
        if self.symbols.len() < self.max {
            let value = json!({"name":name,"kind":kind,"byte_start":range.start,"byte_end":range.end,"start_line":self.lines.line(range.start),"end_line":self.lines.last_line(range.start,range.end),"parent":parent});
            self.output.add(&value)?;
            self.symbols.push(value);
        }
        Ok(id)
    }
    fn items(
        &mut self,
        items: &[Item],
        parent: Option<usize>,
        depth: usize,
    ) -> Result<(), CapabilityError> {
        if depth > self.depth {
            return Err(failure("outline nesting limit exceeded"));
        }
        for item in items {
            self.context.check()?;
            let (name, kind) = match item {
                Item::Const(i) => (i.ident.to_string(), "constant"),
                Item::Enum(i) => (i.ident.to_string(), "enum"),
                Item::ExternCrate(i) => (
                    i.rename
                        .as_ref()
                        .map(|(_, name)| name)
                        .unwrap_or(&i.ident)
                        .to_string(),
                    "extern_crate",
                ),
                Item::Fn(i) => (i.sig.ident.to_string(), "function"),
                Item::ForeignMod(i) => (self.source(i.abi.span())?, "extern_block"),
                Item::Impl(i) => (self.source(i.self_ty.span())?, "impl"),
                Item::Macro(i) => (
                    i.ident
                        .as_ref()
                        .map(|i| i.to_string())
                        .unwrap_or(self.source(i.mac.path.span())?),
                    "macro",
                ),
                Item::Mod(i) => (i.ident.to_string(), "module"),
                Item::Static(i) => (i.ident.to_string(), "static"),
                Item::Struct(i) => (i.ident.to_string(), "struct"),
                Item::Trait(i) => (i.ident.to_string(), "trait"),
                Item::TraitAlias(i) => (i.ident.to_string(), "trait_alias"),
                Item::Type(i) => (i.ident.to_string(), "type"),
                Item::Union(i) => (i.ident.to_string(), "union"),
                Item::Use(i) => (self.source(i.tree.span())?, "use"),
                _ => return Err(failure("unsupported Rust outline item")),
            };
            let id = self.add(name, kind, item.span(), parent)?;
            match item {
                Item::Mod(i) => {
                    if let Some((_, items)) = &i.content {
                        self.items(items, Some(id), depth + 1)?;
                    }
                }
                Item::Impl(i) => {
                    for member in &i.items {
                        let (name, kind) = match member {
                            ImplItem::Fn(i) => (i.sig.ident.to_string(), "function"),
                            ImplItem::Const(i) => (i.ident.to_string(), "associated_constant"),
                            ImplItem::Type(i) => (i.ident.to_string(), "associated_type"),
                            ImplItem::Macro(i) => (self.source(i.mac.path.span())?, "macro"),
                            _ => return Err(failure("unsupported Rust impl item")),
                        };
                        self.add(name, kind, member.span(), Some(id))?;
                    }
                }
                Item::Trait(i) => {
                    for member in &i.items {
                        let (name, kind) = match member {
                            TraitItem::Fn(i) => (i.sig.ident.to_string(), "function"),
                            TraitItem::Const(i) => (i.ident.to_string(), "associated_constant"),
                            TraitItem::Type(i) => (i.ident.to_string(), "associated_type"),
                            TraitItem::Macro(i) => (self.source(i.mac.path.span())?, "macro"),
                            _ => return Err(failure("unsupported Rust trait item")),
                        };
                        self.add(name, kind, member.span(), Some(id))?;
                    }
                }
                Item::ForeignMod(i) => {
                    for member in &i.items {
                        let (name, kind) = match member {
                            syn::ForeignItem::Fn(i) => (i.sig.ident.to_string(), "function"),
                            syn::ForeignItem::Static(i) => (i.ident.to_string(), "static"),
                            syn::ForeignItem::Type(i) => (i.ident.to_string(), "type"),
                            syn::ForeignItem::Macro(i) => {
                                (self.source(i.mac.path.span())?, "macro")
                            }
                            _ => return Err(failure("unsupported Rust foreign item")),
                        };
                        self.add(name, kind, member.span(), Some(id))?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn source(&self, span: Span) -> Result<String, CapabilityError> {
        let range = span.byte_range();
        self.text
            .get(range.start + self.source_offset..range.end + self.source_offset)
            .map(str::to_owned)
            .ok_or_else(|| failure("invalid Rust source range"))
    }
}

struct DiffLine<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}
#[derive(Clone, Copy)]
struct Range {
    start: u64,
    count: u64,
    end: u64,
}
impl Range {
    fn json(self) -> Value {
        json!({"start_line":self.start,"line_count":self.count,"end_line_exclusive":self.end})
    }
}
fn number(text: &str) -> Result<u64, CapabilityError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(failure("malformed diff hunk counter"));
    }
    text.parse()
        .map_err(|_| failure("oversized diff hunk counter"))
}
fn range(text: &str, sign: char) -> Result<Range, CapabilityError> {
    let text = text
        .strip_prefix(sign)
        .ok_or_else(|| failure("malformed diff hunk range"))?;
    let (start, count) = if let Some((start, count)) = text.split_once(',') {
        (number(start)?, number(count)?)
    } else {
        (number(text)?, 1)
    };
    if count > 0 && start == 0 {
        return Err(failure("nonempty diff range must start at line 1 or later"));
    }
    let end = start
        .checked_add(count)
        .filter(|end| *end <= u32::MAX as u64)
        .ok_or_else(|| failure("oversized diff line range"))?;
    Ok(Range { start, count, end })
}
fn hunk_header(text: &str) -> Result<(Range, Range), CapabilityError> {
    let rest = text
        .strip_prefix("@@ ")
        .ok_or_else(|| failure("malformed unified diff hunk header"))?;
    let (ranges, trailing) = rest
        .split_once(" @@")
        .ok_or_else(|| failure("malformed unified diff hunk terminator"))?;
    if !trailing.is_empty() && !trailing.starts_with(' ') {
        return Err(failure("malformed unified diff section label"));
    }
    let (old, new) = ranges
        .split_once(' ')
        .ok_or_else(|| failure("malformed unified diff ranges"))?;
    Ok((range(old, '-')?, range(new, '+')?))
}
fn diff_path(header: &str, marker: &str, prefix: &str) -> Result<Option<String>, CapabilityError> {
    let path = header
        .strip_prefix(marker)
        .ok_or_else(|| failure("missing unified diff file header"))?
        .split('\t')
        .next()
        .unwrap_or_default();
    if path == "/dev/null" {
        return Ok(None);
    }
    let path = path.strip_prefix(prefix).unwrap_or(path);
    filesystem::components(path, false)
        .map_err(|error| failure(format!("invalid diff path: {}", error.message)))?;
    Ok(Some(path.into()))
}
fn diff(
    text: &str,
    max: usize,
    workspace: &Workspace,
    context: &Context,
) -> Result<Value, CapabilityError> {
    let mut lines = vec![];
    let mut at = 0;
    for raw in text.split_inclusive('\n') {
        context.check()?;
        if lines.len() >= workspace.limits.max_tree_entries {
            return Err(failure("diff input line limit exceeded"));
        }
        let line = raw
            .strip_suffix('\n')
            .unwrap_or(raw)
            .strip_suffix('\r')
            .unwrap_or(raw.strip_suffix('\n').unwrap_or(raw));
        lines.push(DiffLine {
            text: line,
            start: at,
            end: at + raw.len(),
        });
        at += raw.len();
    }
    let mut hunks: Vec<Value> = vec![];
    let mut section_hunks = 0;
    let mut paths: Option<(Option<String>, Option<String>)> = None;
    let mut git_paths: Option<(Option<String>, Option<String>)> = None;
    let mut at = 0;
    let mut hunk_count = 0;
    let mut last_old = 0;
    let mut last_new = 0;
    let mut old_eof = false;
    let mut new_eof = false;
    let mut output = OutputBudget::new(workspace.limits.max_output_bytes);
    while at < lines.len() {
        context.check()?;
        let line = lines[at].text;
        if line.starts_with("diff --git ") {
            if paths.take().is_some() && section_hunks == 0 {
                return Err(failure("diff file has no text hunks"));
            }
            if git_paths.is_some() {
                return Err(failure("Git diff metadata has no file headers"));
            }
            let Some((old, new)) = line
                .strip_prefix("diff --git ")
                .and_then(|s| s.split_once(' '))
            else {
                return Err(failure("malformed Git diff paths"));
            };
            git_paths = Some((
                diff_path(&format!("--- {old}"), "--- ", "a/")?,
                diff_path(&format!("+++ {new}"), "+++ ", "b/")?,
            ));
            at += 1;
            continue;
        }
        if line.starts_with("index ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
        {
            if git_paths.is_none() || paths.is_some() {
                return Err(failure("misplaced Git diff metadata"));
            }
            at += 1;
            continue;
        }
        if line.starts_with("--- ") {
            if paths.take().is_some() && section_hunks == 0 {
                return Err(failure("diff file has no text hunks"));
            }
            let old = diff_path(line, "--- ", "a/")?;
            let new = diff_path(
                lines
                    .get(at + 1)
                    .ok_or_else(|| failure("missing new diff file header"))?
                    .text,
                "+++ ",
                "b/",
            )?;
            if old.is_none() && new.is_none() {
                return Err(failure("both diff paths are /dev/null"));
            }
            if let Some((git_old, git_new)) = git_paths.take()
                && (old.as_ref().is_some_and(|p| Some(p) != git_old.as_ref())
                    || new.as_ref().is_some_and(|p| Some(p) != git_new.as_ref()))
            {
                return Err(failure("Git and unified diff paths disagree"));
            }
            let header = json!({"old_path":old,"new_path":new});
            output.add(&header)?;
            paths = Some((old, new));
            section_hunks = 0;
            last_old = 0;
            last_new = 0;
            old_eof = false;
            new_eof = false;
            at += 2;
            continue;
        }
        if line.starts_with("@@") {
            let (old_path, new_path) = paths
                .as_ref()
                .ok_or_else(|| failure("hunk has no file headers"))?;
            let (old, new) = hunk_header(line)?;
            let old_begin = old.start + u64::from(old.count == 0);
            let new_begin = new.start + u64::from(new.count == 0);
            if old_begin < last_old
                || new_begin < last_new
                || (old.count == 0 && new.count == 0)
                || (old_path.is_none() && (old.start != 0 || old.count != 0))
                || (new_path.is_none() && (new.start != 0 || new.count != 0))
            {
                return Err(failure("overlapping or invalid diff hunk ranges"));
            }
            // An ended side can only anchor an empty range at its EOF while
            // the other side continues, including in later hunks of this file.
            if (old_eof && (old.count != 0 || old_begin != last_old))
                || (new_eof && (new.count != 0 || new_begin != last_new))
            {
                return Err(failure("diff hunk range continues after no-newline marker"));
            }
            last_old = old_begin + old.count;
            last_new = new_begin + new.count;
            hunk_count += 1;
            section_hunks += 1;
            if hunk_count > workspace.limits.max_tree_entries {
                return Err(failure("diff hunk limit exceeded"));
            }
            let header_at = at;
            at += 1;
            let mut old_count = 0;
            let mut new_count = 0;
            let mut previous_sides = None;
            while at < lines.len() {
                context.check()?;
                let body = lines[at].text;
                if body == "\\ No newline at end of file" {
                    let Some((has_old, has_new)) = previous_sides.take() else {
                        return Err(failure("misplaced diff no-newline marker"));
                    };
                    old_eof |= has_old;
                    new_eof |= has_new;
                    at += 1;
                    continue;
                }
                if old_count == old.count && new_count == new.count {
                    break;
                }
                let first = body
                    .as_bytes()
                    .first()
                    .ok_or_else(|| failure("diff body line has no prefix"))?;
                let (has_old, has_new) = match first {
                    b' ' => (true, true),
                    b'-' => (true, false),
                    b'+' => (false, true),
                    _ => return Err(failure("invalid or incomplete diff hunk body")),
                };
                if (has_old && old_eof) || (has_new && new_eof) {
                    return Err(failure("diff body continues after no-newline marker"));
                }
                old_count += u64::from(has_old);
                new_count += u64::from(has_new);
                if old_count > old.count || new_count > new.count {
                    return Err(failure("diff hunk body exceeds declared counters"));
                }
                previous_sides = Some((has_old, has_new));
                at += 1;
            }
            if old_count != old.count || new_count != new.count {
                return Err(failure("diff hunk counters do not match body"));
            }
            let end = if at > header_at + 1 {
                lines[at - 1].end
            } else {
                lines[header_at].end
            };
            if hunks.len() < max {
                let value = json!({"old_path":old_path,"new_path":new_path,"old":old.json(),"new":new.json(),"header_line":header_at+1,"header_byte_start":lines[header_at].start,"header_byte_end":lines[header_at].end,"body_start_line":header_at+2,"body_end_line_exclusive":at+1,"byte_start":lines[header_at].start,"byte_end":end});
                output.add(&value)?;
                hunks.push(value);
            }
            continue;
        }
        if line.is_empty() {
            at += 1;
            continue;
        }
        return Err(failure("unexpected or unsupported unified diff content"));
    }
    if paths.is_some() && section_hunks == 0 {
        return Err(failure("diff file has no text hunks"));
    }
    if git_paths.is_some() {
        return Err(failure("Git diff metadata has no text file headers"));
    }
    context.check()?;
    Ok(
        json!({"sha256":filesystem::hash(text.as_bytes()),"hunks":hunks,"total_hunks":hunk_count,"truncated":hunk_count > max,"validated_complete":true}),
    )
}
