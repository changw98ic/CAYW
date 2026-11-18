//! Static Rust paths. These nodes never enter the execution or replay engine.

use std::{
    io::Read,
    ops::Range,
    path::{Component, Path},
};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::Serialize;
use syn::{
    Block, Expr, Item, Stmt,
    spanned::Spanned,
    visit::{self, Visit},
};

use crate::Error;

#[derive(Serialize)]
pub(crate) struct SourceView {
    path: String,
    digest: String,
    source: String,
    functions: Vec<Function>,
    graph: Option<Graph>,
}

#[derive(Serialize)]
struct Function {
    name: String,
    location: Location,
}

#[derive(Clone, Serialize)]
struct Location {
    line: usize,
    column: usize,
    bytes: Range<usize>,
}

impl Location {
    fn new(span: proc_macro2::Span) -> Self {
        Self {
            line: span.start().line,
            column: span.start().column + 1,
            bytes: span.byte_range(),
        }
    }
}

#[derive(Default, Serialize)]
struct Graph {
    entry: usize,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

#[derive(Serialize)]
struct Node {
    kind: &'static str,
    location: Location,
    detail: Option<&'static str>,
    target: Option<CallTarget>,
}

#[derive(Serialize)]
struct CallTarget {
    path: String,
    digest: String,
    location: Location,
}

#[derive(Serialize)]
struct Edge {
    from: usize,
    to: usize,
    kind: &'static str,
}

struct Body<'a> {
    name: &'a syn::Ident,
    block: &'a Block,
    top_level: bool,
}

#[derive(Default)]
struct Bodies<'a>(Vec<Body<'a>>);

impl<'a> Visit<'a> for Bodies<'a> {
    fn visit_item_fn(&mut self, item: &'a syn::ItemFn) {
        self.0.push(Body {
            name: &item.sig.ident,
            block: &item.block,
            top_level: false,
        });
        visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'a syn::ImplItemFn) {
        self.0.push(Body {
            name: &item.sig.ident,
            block: &item.block,
            top_level: false,
        });
        visit::visit_impl_item_fn(self, item);
    }

    fn visit_trait_item_fn(&mut self, item: &'a syn::TraitItemFn) {
        if let Some(block) = &item.default {
            self.0.push(Body {
                name: &item.sig.ident,
                block,
                top_level: false,
            });
        }
        visit::visit_trait_item_fn(self, item);
    }
}

pub(crate) fn inspect(
    root: &Dir,
    path: String,
    selection: Option<(usize, String)>,
) -> Result<SourceView, Error> {
    let source = read_source(root, &path)?;
    let digest = cayw_codeintel::content_sha256(source.as_bytes());
    if let Some((_, expected)) = &selection
        && expected != &digest
    {
        return Err(Error::Conflict("源码已更改，请重新加载文件".into()));
    }
    analyze(
        root,
        path,
        digest,
        source,
        selection.map(|(offset, _)| offset),
    )
}

fn read_source(root: &Dir, path: &str) -> Result<String, Error> {
    let relative = Path::new(path);
    if relative.extension().and_then(|ext| ext.to_str()) != Some("rs") {
        return Err(Error::Invalid("请输入工作区内的 .rs 文件路径".into()));
    }
    let mut dir = root.try_clone()?;
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            return Err(Error::Invalid("请使用不含上级目录的工作区相对路径".into()));
        };
        if parts.peek().is_some() {
            dir = dir.open_dir_nofollow(name)?;
        } else {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No).nonblock(true);
            let mut file = dir.open_with(name, &options)?;
            if !file.metadata()?.is_file() {
                return Err(Error::Invalid("源码路径必须指向普通文件".into()));
            }
            let mut source = String::new();
            file.read_to_string(&mut source)?;
            return Ok(source);
        }
    }
    Err(Error::Invalid("需要 Rust 文件路径".into()))
}

fn parse(source: &str) -> syn::Result<syn::File> {
    // Preserve byte offsets when skipping a BOM and executable Rust's shebang.
    let tail = source.strip_prefix('\u{feff}').unwrap_or(source);
    let prefix = source.len() - tail.len();
    let shebang = if tail.starts_with("#!") && !tail[2..].trim_start().starts_with('[') {
        tail.split('\n')
            .next()
            .expect("split always yields one item")
            .len()
    } else {
        0
    };
    let skip = prefix + shebang;
    if skip == 0 {
        syn::parse_str(source)
    } else {
        syn::parse_str(&format!("{}{}", " ".repeat(skip), &source[skip..]))
    }
}

fn analyze(
    root: &Dir,
    path: String,
    digest: String,
    source: String,
    selected: Option<usize>,
) -> Result<SourceView, Error> {
    let syntax = parse(&source).map_err(|error| {
        Error::Invalid(format!(
            "无法解析 Rust 文件，第 {} 行：{error}",
            error.span().start().line
        ))
    })?;
    let mut bodies = Bodies::default();
    for item in &syntax.items {
        let index = bodies.0.len();
        bodies.visit_item(item);
        if matches!(item, Item::Fn(_)) {
            bodies.0[index].top_level = true;
        }
    }
    let functions = bodies
        .0
        .iter()
        .map(|body| Function {
            name: body.name.to_string(),
            location: Location::new(body.name.span()),
        })
        .collect::<Vec<_>>();
    let graph = selected
        .map(|offset| {
            let index = functions
                .iter()
                .position(|function| function.location.bytes.start == offset)
                .ok_or_else(|| Error::Invalid("当前源码版本中没有选定的函数".into()))?;
            let body = &bodies.0[index];
            let mut builder = Builder {
                graph: Graph::default(),
                top_level: body.top_level,
                source: SourceContext {
                    root,
                    path: &path,
                    digest: &digest,
                    items: &syntax.items,
                    modules: Default::default(),
                },
                exit: 0,
                targets: Vec::new(),
            };
            builder.exit = builder.node("exit", body.block.brace_token.span.close());
            let tail = match body.block.stmts.last() {
                Some(Stmt::Expr(expr, None)) => expr.span(),
                _ => body.block.brace_token.span.close(),
            };
            let end = builder.step("implicit_return", tail, builder.exit);
            let start = builder.block(body.block, end);
            builder.graph.entry = builder.step("entry", body.name.span(), start);
            Ok::<_, Error>(builder.graph)
        })
        .transpose()?;
    Ok(SourceView {
        path,
        digest,
        source,
        functions,
        graph,
    })
}

struct Target {
    label: Option<String>,
    leave: usize,
    again: Option<usize>,
}

struct SourceContext<'a> {
    root: &'a Dir,
    path: &'a str,
    digest: &'a str,
    items: &'a [Item],
    modules: std::collections::BTreeMap<String, Option<(String, String, syn::File)>>,
}

impl SourceContext<'_> {
    fn external_module(&mut self, name: &syn::Ident) -> Option<&(String, String, syn::File)> {
        let name_text = name.to_string();
        self.modules
            .entry(name_text.clone())
            .or_insert_with(|| {
                let mut declarations = self.items.iter().filter_map(|item| match item {
                    Item::Mod(module) if module.ident == *name => Some(module),
                    _ => None,
                });
                let module = declarations.next()?;
                if module.content.is_some()
                    || !module.attrs.is_empty()
                    || declarations.next().is_some()
                {
                    return None;
                }
                let source_path = Path::new(self.path);
                let parent = source_path.parent()?;
                // The input path is explicit. For other file names, the actual
                // module directory can depend on the crate target or #[path].
                if !matches!(
                    source_path.file_name()?.to_str()?,
                    "lib.rs" | "main.rs" | "mod.rs"
                ) {
                    return None;
                }
                let name = name_text.strip_prefix("r#").unwrap_or(&name_text);
                let paths = [
                    parent.join(format!("{name}.rs")),
                    parent.join(name).join("mod.rs"),
                ];
                let mut found = None;
                for path in paths {
                    let path = path.into_os_string().into_string().ok()?;
                    match read_source(self.root, &path) {
                        Ok(source) if found.is_none() => {
                            let syntax = parse(&source).ok()?;
                            let digest = cayw_codeintel::content_sha256(source.as_bytes());
                            found = Some((path, digest, syntax));
                        }
                        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                        _ => return None,
                    }
                }
                found
            })
            .as_ref()
    }
}

fn direct_function<'a>(items: &'a [Item], name: &syn::Ident) -> Option<&'a syn::ItemFn> {
    let mut matches = items.iter().filter_map(|item| match item {
        Item::Fn(function) if function.sig.ident == *name => Some(function),
        _ => None,
    });
    let candidate = matches.next()?;
    (candidate
        .attrs
        .iter()
        .all(|attr| !attr.path().is_ident("cfg") && !attr.path().is_ident("cfg_attr"))
        && matches.next().is_none())
    .then_some(candidate)
}

struct Builder<'a> {
    graph: Graph,
    top_level: bool,
    source: SourceContext<'a>,
    exit: usize,
    targets: Vec<Target>,
}

// Collect only immediate expression children. The builder visits them in Rust
// evaluation order and handles bodies and conditional expressions separately.
#[derive(Default)]
struct Children<'a>(Vec<&'a Expr>);

impl<'a> Visit<'a> for Children<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        self.0.push(expr);
    }
    fn visit_block(&mut self, _: &'a Block) {}
    fn visit_type(&mut self, _: &'a syn::Type) {}
    fn visit_pat(&mut self, _: &'a syn::Pat) {}
}

impl Builder<'_> {
    fn node(&mut self, kind: &'static str, span: proc_macro2::Span) -> usize {
        let id = self.graph.nodes.len();
        self.graph.nodes.push(Node {
            kind,
            location: Location::new(span),
            detail: None,
            target: None,
        });
        id
    }

    fn edge(&mut self, from: usize, to: usize, kind: &'static str) {
        self.graph.edges.push(Edge { from, to, kind });
    }

    fn step(&mut self, kind: &'static str, span: proc_macro2::Span, next: usize) -> usize {
        let node = self.node(kind, span);
        self.edge(node, next, "next");
        node
    }

    fn block(&mut self, block: &Block, next: usize) -> usize {
        block
            .stmts
            .iter()
            .rev()
            .fold(next, |next, stmt| match stmt {
                Stmt::Expr(expr, _) => self.expr(expr, next),
                Stmt::Local(local) => match &local.init {
                    Some(init) => {
                        let after_init = if let Some((_, diverge)) = &init.diverge {
                            let test = self.node("let_else", local.pat.span());
                            let unknown = self.node("diverge", diverge.span());
                            self.graph.nodes[unknown].detail =
                                Some("语法要求此处离开当前路径，具体终点无法确认");
                            let failure = self.expr(diverge, unknown);
                            self.edge(test, next, "match");
                            self.edge(test, failure, "no_match");
                            test
                        } else {
                            next
                        };
                        self.expr(&init.expr, after_init)
                    }
                    None => next,
                },
                Stmt::Macro(mac) => self.macro_step(&mac.mac, next),
                Stmt::Item(_) => next,
            })
    }

    fn macro_step(&mut self, mac: &syn::Macro, next: usize) -> usize {
        let node = self.step("macro", mac.span(), next);
        self.graph.nodes[node].detail = Some("宏未展开，内部调用和控制流未知");
        node
    }

    fn condition(&mut self, expr: &Expr, yes: usize, no: usize) -> usize {
        match expr {
            Expr::Binary(binary) if matches!(binary.op, syn::BinOp::And(_)) => {
                let rhs = self.condition(&binary.right, yes, no);
                self.condition(&binary.left, rhs, no)
            }
            Expr::Binary(binary) if matches!(binary.op, syn::BinOp::Or(_)) => {
                let rhs = self.condition(&binary.right, yes, no);
                self.condition(&binary.left, yes, rhs)
            }
            Expr::Paren(paren) => self.condition(&paren.expr, yes, no),
            _ => {
                let test = self.node("condition", expr.span());
                self.edge(test, yes, "true");
                self.edge(test, no, "false");
                self.expr(expr, test)
            }
        }
    }

    fn target(&self, label: Option<&syn::Lifetime>, again: bool) -> Option<usize> {
        self.targets.iter().rev().find_map(|target| {
            if let Some(label) = label {
                if target.label.as_deref() != Some(&label.ident.to_string()) {
                    return None;
                }
            } else if target.again.is_none() {
                return None;
            }
            if again {
                target.again
            } else {
                Some(target.leave)
            }
        })
    }

    fn loop_body(
        &mut self,
        block: &Block,
        label: Option<&syn::Label>,
        head: usize,
        next: usize,
    ) -> usize {
        self.targets.push(Target {
            label: label.map(|label| label.name.ident.to_string()),
            leave: next,
            again: Some(head),
        });
        let back = self.node("iteration_end", block.brace_token.span.close());
        self.edge(back, head, "repeat");
        let body = self.block(block, back);
        self.targets.pop();
        body
    }

    fn call(&mut self, call: &Expr, callee: Option<&Expr>, next: usize) -> usize {
        let span = match call {
            Expr::MethodCall(method) => method.method.span(),
            _ => call.span(),
        };
        let node = self.step("call", span, next);
        let target = match callee {
            Some(Expr::Path(path))
                if self.top_level && path.qself.is_none() && path.path.leading_colon.is_none() =>
            {
                let mut segments = path.path.segments.iter();
                match (
                    segments.next(),
                    segments.next(),
                    segments.next(),
                    segments.next(),
                ) {
                    (Some(prefix), Some(name), None, None) if prefix.ident == "self" => {
                        direct_function(self.source.items, &name.ident).map(|target| CallTarget {
                            path: self.source.path.into(),
                            digest: self.source.digest.into(),
                            location: Location::new(target.sig.ident.span()),
                        })
                    }
                    (Some(prefix), Some(module), Some(name), None) if prefix.ident == "self" => {
                        self.source.external_module(&module.ident).and_then(
                            |(path, digest, syntax)| {
                                direct_function(&syntax.items, &name.ident).map(|target| {
                                    CallTarget {
                                        path: path.clone(),
                                        digest: digest.clone(),
                                        location: Location::new(target.sig.ident.span()),
                                    }
                                })
                            },
                        )
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(target) = target {
            self.graph.nodes[node].target = Some(target);
        } else {
            self.graph.nodes[node].detail = Some("调用目标需要名称或类型解析");
        }
        self.children(call, node)
    }

    fn children(&mut self, expr: &Expr, next: usize) -> usize {
        let mut children = Children::default();
        visit::visit_expr(&mut children, expr);
        children
            .0
            .into_iter()
            .rev()
            .fold(next, |next, child| self.expr(child, next))
    }

    fn expr(&mut self, expr: &Expr, next: usize) -> usize {
        match expr {
            Expr::If(value) => {
                let yes = self.block(&value.then_branch, next);
                let no = value
                    .else_branch
                    .as_ref()
                    .map_or(next, |(_, otherwise)| self.expr(otherwise, next));
                self.condition(&value.cond, yes, no)
            }
            Expr::Match(value) => {
                let mut miss = self.node("match_end", value.brace_token.span.close());
                for arm in value.arms.iter().rev() {
                    let body = self.expr(&arm.body, next);
                    let hit = arm
                        .guard
                        .as_ref()
                        .map_or(body, |(_, guard)| self.condition(guard, body, miss));
                    let test = self.node("pattern", arm.pat.span());
                    self.edge(test, hit, "match");
                    if !matches!(arm.pat, syn::Pat::Wild(_)) {
                        self.edge(test, miss, "no_match");
                    }
                    miss = test;
                }
                let dispatch = self.step("match", value.match_token.span, miss);
                self.expr(&value.expr, dispatch)
            }
            Expr::Loop(value) => {
                let head = self.node("loop", value.loop_token.span);
                let body = self.loop_body(&value.body, value.label.as_ref(), head, next);
                self.edge(head, body, "next");
                head
            }
            Expr::While(value) => {
                let head = self.node("while", value.while_token.span);
                let body = self.loop_body(&value.body, value.label.as_ref(), head, next);
                let test = self.condition(&value.cond, body, next);
                self.edge(head, test, "next");
                head
            }
            Expr::ForLoop(value) => {
                let head = self.node("for", value.for_token.span);
                let body = self.loop_body(&value.body, value.label.as_ref(), head, next);
                self.edge(head, body, "next_item");
                self.edge(head, next, "exhausted");
                self.expr(&value.expr, head)
            }
            Expr::Return(value) => {
                let node = self.node("return", value.return_token.span);
                self.edge(node, self.exit, "return");
                value
                    .expr
                    .as_ref()
                    .map_or(node, |value| self.expr(value, node))
            }
            Expr::Try(value) => {
                let node = self.node("try", value.question_token.span);
                self.edge(node, next, "continue");
                self.edge(node, self.exit, "early_return");
                self.expr(&value.expr, node)
            }
            Expr::Break(value) => {
                let node = self.node("break", value.break_token.span);
                if let Some(to) = self.target(value.label.as_ref(), false) {
                    self.edge(node, to, "break");
                }
                value
                    .expr
                    .as_ref()
                    .map_or(node, |value| self.expr(value, node))
            }
            Expr::Continue(value) => {
                let node = self.node("continue", value.continue_token.span);
                if let Some(to) = self.target(value.label.as_ref(), true) {
                    self.edge(node, to, "repeat");
                }
                node
            }
            Expr::Call(value) => self.call(expr, Some(&value.func), next),
            Expr::MethodCall(_) => self.call(expr, None, next),
            Expr::Macro(value) => self.macro_step(&value.mac, next),
            Expr::Block(value) => {
                if let Some(label) = &value.label {
                    self.targets.push(Target {
                        label: Some(label.name.ident.to_string()),
                        leave: next,
                        again: None,
                    });
                    let start = self.block(&value.block, next);
                    self.targets.pop();
                    start
                } else {
                    self.block(&value.block, next)
                }
            }
            Expr::Unsafe(value) => self.block(&value.block, next),
            Expr::Binary(value) if matches!(value.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) => {
                self.condition(expr, next, next)
            }
            Expr::Assign(value) => {
                let lhs = self.expr(&value.left, next);
                self.expr(&value.right, lhs)
            }
            Expr::Closure(_)
            | Expr::Async(_)
            | Expr::Const(_)
            | Expr::TryBlock(_)
            | Expr::Verbatim(_) => {
                let node = self.step("opaque", expr.span(), next);
                self.graph.nodes[node].detail = Some("此表达式的内部控制流尚未展开");
                node
            }
            _ => self.children(expr, next),
        }
    }
}
