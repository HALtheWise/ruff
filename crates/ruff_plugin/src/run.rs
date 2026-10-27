//! Running plugins while ruff lints a file.
//!
//! Ruff calls [`FileRun::visit_stmt`] and [`FileRun::visit_expr`] as it visits each node, before
//! visiting the node's children. The run finds the checks that select the node, and calls their
//! callbacks with a [`NodeValue`]. Callbacks can report diagnostics, and register more checks
//! (scoped to the nodes they receive), which see the nodes that ruff visits afterwards.
//!
//! Each plugin gets one Starlark heap per file, which holds the state that its callbacks build up
//! (in the variables of their enclosing functions) and the callbacks they register. The heap is
//! dropped when the file is done.

use std::borrow::Cow;
use std::cell::{Cell, OnceCell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ruff_diagnostics::{Edit, Fix, IsolationLevel};
use ruff_python_ast::helpers::resolve_imported_module_path;
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt};
use ruff_source_file::{LineRanges, SourceFile};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use starlark::any::ProvidesStaticType;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::Value;
use starlark::values::list::{AllocList, ListRef};

use crate::edit::EditValue;
use crate::kind::{Kind, KindSet};
use crate::node::{NodeKey, NodeRef};
use crate::selector::{Filter, QUALIFIED, Scope, Selector, Source};
use crate::values::{Data, ModuleValue, NodeValue, name_of};
use crate::{Finding, Host, Inner, PluginError};

/// The isolation groups of plugin fixes, which are distinct from those of ruff's own fixes (the
/// ids of nodes in ruff's semantic model).
const ISOLATION_GROUPS: u32 = 1 << 31;

/// A callback, as an index into a list of functions: the plugin's frozen list for callbacks
/// registered while it loaded, and the file's list for those registered while linting.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Callback {
    Frozen(usize),
    File(usize),
}

/// One branch of a selector, registered with a callback in the file being linted.
struct Check {
    plugin: usize,
    callback: Callback,
    selector: Arc<Selector>,
    branch: usize,
    /// The node that the check is scoped to, or `None` for the whole file.
    scope: Option<u32>,
    /// The checks of one call to `foreach` share a group, so that a node that several branches
    /// select is only passed to the callback once, and a failing callback is only reported once.
    group: usize,
}

impl Check {
    fn source(&self) -> &Source {
        &self.selector.branches[self.branch].source
    }

    fn filters(&self) -> &[Filter] {
        &self.selector.branches[self.branch].filters
    }

    fn selects(&self, kind: Kind) -> bool {
        matches!(self.source(), Source::Nodes(kinds) if kinds.contains(kind))
    }
}

/// An `on_exit` callback.
struct Exit {
    plugin: usize,
    callback: usize,
    /// The node whose exit the callback waits for, or `None` for the module.
    scope: Option<u32>,
}

/// A node that a plugin has seen.
pub(crate) struct Entry<'a> {
    pub(crate) node: NodeRef<'a>,
    pub(crate) parent: Option<u32>,
    /// The qualified name of a name, attribute, or call, resolved when ruff visited the node.
    qualname: OnceCell<Option<String>>,
}

#[derive(Default)]
struct State<'a> {
    nodes: Vec<Entry<'a>>,
    ids: FxHashMap<NodeKey, u32>,
    checks: Vec<Check>,
    /// Whether each group of checks has failed in this file.
    failed: Vec<bool>,
    /// The callbacks that have failed in this file, so that other registrations of the same
    /// function (in other scopes, say) aren't called either.
    failed_callbacks: Vec<(usize, Callback)>,
    /// Checks of nodes for the whole file, by the kind of node they select.
    by_kind: Vec<Vec<usize>>,
    /// Checks of nodes scoped to a statement or expression, by the id of that node.
    scoped: FxHashMap<u32, Vec<usize>>,
    /// Checks of nodes scoped to other nodes, such as parameters, which aren't among the
    /// ancestors that ruff's semantic model tracks, by the id of the innermost statement or
    /// expression that contains the scope. Their nodes are found by position within it.
    scoped_within: FxHashMap<u32, Vec<usize>>,
    /// Checks scoped to nodes that aren't inside any statement or expression.
    scoped_by_range: Vec<usize>,
    /// The kinds of nodes that any scoped check selects.
    scoped_kinds: KindSet,
    imports: Vec<usize>,
    comments: Vec<usize>,
    modules: Vec<usize>,
    exits: Vec<Exit>,
    findings: Vec<Finding<'a>>,
    errors: Vec<PluginError<'a>>,
    /// Whether ruff has finished traversing the file's nodes, so that no more checks can be
    /// registered.
    traversed: bool,
    /// Whether the `on_exit` callbacks are running, so that no more of them can be registered.
    finished: bool,
}

/// The state of the plugins while ruff lints one file.
pub struct FileRun<'a> {
    inner: &'a Inner,
    /// Identifies this run among all runs in the process, so that node values can't be used with
    /// another file's nodes.
    pub(crate) generation: u64,
    /// The path of the file relative to the root of each plugin, with `/` separators.
    pub(crate) paths: Vec<String>,
    /// The dotted name of the module, if it's in a package.
    pub(crate) module_name: Option<String>,
    module_path: Option<Vec<String>>,
    /// Each plugin's Starlark module for this file, which owns its heap.
    heaps: Vec<OnceCell<Module>>,
    started: Cell<bool>,
    /// The file being linted, for the values of its nodes.
    source_file: OnceCell<SourceFile>,
    /// The kinds of nodes that any check selects. This and `child_checks` are checked for every
    /// node, so they're kept outside of `state`, which costs a borrow to read.
    kinds: Cell<KindSet>,
    /// Whether any check selects nodes that ruff doesn't visit on their own (see
    /// [`is_child_kind`]).
    child_checks: Cell<bool>,
    has_imports: Cell<bool>,
    state: RefCell<State<'a>>,
    /// The first segments that qualified names in the file can have so far: those of the
    /// modules it has imported, `builtins`, and its own package. Like the `seen_module` checks of
    /// ruff's own rules, this lets a check of `calls("requests.*")` skip resolving names in files
    /// that don't import `requests`.
    roots: RefCell<FxHashSet<String>>,
}

/// Where a node is, relative to the node that ruff's semantic model is visiting.
#[derive(Copy, Clone)]
enum Origin<'r, 'a> {
    /// The node is the one being visited.
    Current,
    /// The node is inside the node being visited, within these nodes (innermost first), which
    /// ruff doesn't visit on their own.
    Within(&'r [NodeRef<'a>]),
}

/// Kinds whose nodes ruff's traversal doesn't visit on their own. They're found among the
/// children of the statements and expressions that ruff does visit.
fn is_child_kind(kind: Kind) -> bool {
    !kind.is_statement() && !kind.is_expression() && kind != Kind::Comment
}

fn to_node_ref(ancestor: crate::Ancestor<'_>) -> NodeRef<'_> {
    match ancestor {
        crate::Ancestor::Stmt(stmt) => NodeRef::Stmt(stmt),
        crate::Ancestor::Expr(expr) => NodeRef::Expr(expr),
    }
}

impl<'a> FileRun<'a> {
    pub(crate) fn new(
        inner: &'a Inner,
        path: &Path,
        module_path: Option<&[String]>,
    ) -> Option<Self> {
        static GENERATION: AtomicU64 = AtomicU64::new(0);

        let paths = inner
            .plugins
            .iter()
            .map(|plugin| {
                path.strip_prefix(&plugin.spec.root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        let run = Self {
            inner,
            generation: GENERATION.fetch_add(1, Ordering::Relaxed),
            paths,
            // A package's `__init__.py` is the module of the package itself.
            module_name: module_path.map(|path| match path {
                [package @ .., last] if last == "__init__" && !package.is_empty() => {
                    package.join(".")
                }
                path => path.join("."),
            }),
            module_path: module_path.map(<[String]>::to_vec),
            heaps: inner.plugins.iter().map(|_| OnceCell::new()).collect(),
            started: Cell::new(false),
            source_file: OnceCell::new(),
            kinds: Cell::new(KindSet::default()),
            child_checks: Cell::new(false),
            has_imports: Cell::new(false),
            roots: RefCell::new(
                [
                    Some("builtins".to_string()),
                    // Classes and functions resolve to names in the module's package (or, in a
                    // script, the module itself).
                    match module_path {
                        Some(path) => path.first().cloned(),
                        None => path
                            .file_stem()
                            .map(|stem| stem.to_string_lossy().into_owned()),
                    },
                ]
                .into_iter()
                .flatten()
                .collect(),
            ),
            state: RefCell::new(State {
                by_kind: vec![Vec::new(); Kind::ALL.len()],
                ..State::default()
            }),
        };
        let mut any = false;
        for (plugin_index, plugin) in inner.plugins.iter().enumerate() {
            for check in &plugin.loaded.checks {
                any |= run.add_check(
                    plugin_index,
                    Callback::Frozen(check.callback),
                    &check.selector,
                    None,
                );
            }
        }
        any.then_some(run)
    }

    /// Registers a check, returning `false` if none of its branches apply to the file.
    fn add_check(
        &self,
        plugin: usize,
        callback: Callback,
        selector: &Arc<Selector>,
        scope: Option<u32>,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        let group = state.failed.len();
        state.failed.push(false);
        let path = &self.paths[plugin];
        // Where to find the nodes of a scoped check: ruff's semantic model tracks the statements
        // and expressions that contain the node being visited, so a check scoped to one of those
        // is found among the node's ancestors. A check scoped to another node (like a function's
        // parameters) is found among the checks of its innermost tracked ancestor.
        let tracked_ancestor = scope.map(|scope| {
            let mut id = Some(scope);
            while let Some(current) = id {
                let entry = &state.nodes[current as usize];
                if matches!(entry.node, NodeRef::Expr(_) | NodeRef::Stmt(_)) {
                    return Some((current, current == scope));
                }
                id = entry.parent;
            }
            None
        });
        let mut added = false;
        for (branch_index, branch) in selector.branches.iter().enumerate() {
            let applies = branch.filters.iter().all(|filter| match filter {
                Filter::Path { globs, exclude } => globs.is_match(path) != *exclude,
                _ => true,
            });
            if !applies {
                continue;
            }
            added = true;
            let index = state.checks.len();
            state.checks.push(Check {
                plugin,
                callback,
                selector: selector.clone(),
                branch: branch_index,
                scope,
                group,
            });
            match &branch.source {
                Source::Nodes(kinds) => {
                    self.kinds.set(self.kinds.get().union(*kinds));
                    if kinds.iter().any(is_child_kind) {
                        self.child_checks.set(true);
                    }
                    if scope.is_some() {
                        state.scoped_kinds = state.scoped_kinds.union(*kinds);
                    }
                    match tracked_ancestor {
                        None => {
                            for kind in kinds.iter() {
                                state.by_kind[kind as usize].push(index);
                            }
                        }
                        Some(Some((scope, true))) => {
                            state.scoped.entry(scope).or_default().push(index);
                        }
                        Some(Some((ancestor, false))) => {
                            state.scoped_within.entry(ancestor).or_default().push(index);
                        }
                        Some(None) => state.scoped_by_range.push(index),
                    }
                }
                Source::Imports => {
                    self.has_imports.set(true);
                    state.imports.push(index);
                }
                Source::Comments => state.comments.push(index),
                Source::Modules => state.modules.push(index),
            }
        }
        added
    }

    fn heap(&self, plugin: usize) -> &Module {
        self.heaps[plugin].get_or_init(|| {
            let module = Module::new();
            module.set_extra_value(module.heap().alloc(AllocList::EMPTY));
            module
        })
    }

    /// Runs the checks of modules. This happens before ruff visits the first node, so that the
    /// checks that their callbacks register see all of the file's nodes.
    fn start(&self, host: &dyn Host<'a>) {
        if self.started.replace(true) {
            return;
        }
        let modules = self.state.borrow().modules.clone();
        for index in modules {
            let (plugin, callback, group, passes) = {
                let state = self.state.borrow();
                let check = &state.checks[index];
                let module_name = self.module_name.as_deref();
                let passes = check.filters().iter().all(|filter| match filter {
                    Filter::Name(regex) => module_name.is_some_and(|name| regex.is_match(name)),
                    Filter::Qualname(patterns) => {
                        module_name.is_some_and(|name| patterns.matches(name))
                    }
                    Filter::Path { .. } | Filter::Text(_) => true,
                });
                (check.plugin, check.callback, check.group, passes)
            };
            if passes {
                self.invoke(
                    host,
                    plugin,
                    callback,
                    Arg::Module,
                    TextRange::default(),
                    Some(group),
                );
            }
        }
    }

    /// Runs the checks for an expression that ruff is visiting.
    pub fn visit_expr(&self, expr: &'a Expr, host: &dyn Host<'a>) {
        self.start(host);
        let node = NodeRef::Expr(expr);
        self.dispatch(node, Origin::Current, host);
        self.visit_children(node, &mut Vec::new(), host);
    }

    /// Runs the checks for a statement that ruff is visiting.
    pub fn visit_stmt(&self, stmt: &'a Stmt, host: &dyn Host<'a>) {
        self.start(host);
        let node = NodeRef::Stmt(stmt);
        self.dispatch(node, Origin::Current, host);
        self.visit_children(node, &mut Vec::new(), host);

        // Python represents each `elif` as a nested `If`.
        if let Stmt::If(ast::StmtIf {
            elif_else_clauses, ..
        }) = stmt
            && self.kinds.get().contains(Kind::If)
        {
            for (index, clause) in elif_else_clauses.iter().enumerate() {
                if clause.test.is_some() {
                    self.dispatch(
                        NodeRef::Elif(&elif_else_clauses[index..]),
                        Origin::Within(&[]),
                        host,
                    );
                }
            }
        }

        self.record_roots(stmt);
        if let Stmt::Import(ast::StmtImport { names, .. })
        | Stmt::ImportFrom(ast::StmtImportFrom { names, .. }) = stmt
            && self.has_imports.get()
        {
            for alias in names {
                self.dispatch_import(stmt, alias, host);
            }
        }
    }

    /// Runs the checks of comments. Call this once, after ruff has visited the file's nodes.
    pub fn visit_comments(
        &self,
        comments: impl IntoIterator<Item = TextRange>,
        host: &dyn Host<'a>,
    ) {
        self.start(host);
        self.state.borrow_mut().traversed = true;
        if self.state.borrow().comments.is_empty() {
            return;
        }
        let source = host.source();
        // Comment checks scoped to nodes, by the range of their node, in source order. A node's
        // range is extended to the end of its last line, so that a comment at the end of that
        // line counts as inside it.
        let (mut scoped, global): (Vec<_>, Vec<_>) = {
            let state = self.state.borrow();
            state
                .comments
                .iter()
                .map(|&index| {
                    let range = state.checks[index]
                        .scope
                        .map_or(TextRange::default(), |scope| {
                            let range = state.nodes[scope as usize].node.range();
                            TextRange::new(range.start(), source.line_end(range.end()))
                        });
                    (range, index)
                })
                .partition(|(_, index)| state.checks[*index].scope.is_some())
        };
        scoped.sort_by_key(|(range, _)| range.start());
        let mut scoped = scoped.into_iter().peekable();
        // The scoped checks whose node may contain the current comment. Nodes nest, so this
        // holds about as many checks as nodes are nested.
        let mut active: Vec<(TextRange, usize)> = Vec::new();
        for comment in comments {
            while let Some(&(range, index)) = scoped.peek()
                && range.start() <= comment.start()
            {
                active.push((range, index));
                scoped.next();
            }
            active.retain(|(range, _)| range.end() >= comment.start());
            let candidates = {
                let state = self.state.borrow();
                let text = &source[comment];
                let mut candidates = Vec::new();
                let in_scope = active
                    .iter()
                    .filter(|(range, _)| range.contains_range(comment));
                for &(_, index) in global.iter().chain(in_scope) {
                    let check = &state.checks[index];
                    let passes = check.filters().iter().all(|filter| match filter {
                        Filter::Text(regexes) => regexes.iter().any(|regex| regex.is_match(text)),
                        _ => true,
                    });
                    if passes && !state.failed[check.group] {
                        candidates.push((check.plugin, check.callback, check.group));
                    }
                }
                candidates
            };
            if candidates.is_empty() {
                continue;
            }
            let id = self.intern(NodeRef::Comment(comment));
            for (plugin, callback, group) in candidates {
                self.invoke(host, plugin, callback, Arg::Node(id), comment, Some(group));
            }
        }
    }

    /// Runs the `on_exit` callbacks, and returns the diagnostics that the plugins reported, and
    /// any errors that their callbacks raised.
    ///
    /// Each node's `on_exit` callbacks run after those of the nodes inside it, and the module's
    /// run last. A node's own callbacks run in the reverse order of their registration.
    pub fn finish(self, host: &dyn Host<'a>) -> (Vec<Finding<'a>>, Vec<PluginError<'a>>) {
        self.start(host);
        let mut exits: Vec<(TextRange, Exit)> = {
            let mut state = self.state.borrow_mut();
            state.traversed = true;
            state.finished = true;
            let exits = std::mem::take(&mut state.exits);
            exits
                .into_iter()
                .rev()
                .map(|exit| {
                    let range = exit.scope.map_or(TextRange::default(), |scope| {
                        state.nodes[scope as usize].node.range()
                    });
                    (range, exit)
                })
                .collect()
        };
        // A node ends no later than the nodes around it, and starts no earlier, so this sort
        // (which is stable) puts inner nodes first. The module's callbacks sort last.
        exits.sort_by_key(|(range, exit)| {
            (
                exit.scope.is_none(),
                range.end(),
                std::cmp::Reverse(range.start()),
            )
        });
        for (range, exit) in exits {
            let callback = Callback::File(exit.callback);
            if self.has_failed(exit.plugin, callback) {
                continue;
            }
            self.invoke(host, exit.plugin, callback, Arg::None, range, None);
        }
        let state = self.state.into_inner();
        (state.findings, state.errors)
    }

    /// Whether `callback` is a function that has already failed in this file.
    fn has_failed(&self, plugin: usize, callback: Callback) -> bool {
        let failed = self.state.borrow().failed_callbacks.clone();
        failed.into_iter().any(|(failed_plugin, failed)| {
            failed_plugin == plugin && self.same_function(plugin, callback, failed)
        })
    }

    /// Whether two callbacks of a plugin are the same function.
    fn same_function(&self, plugin: usize, a: Callback, b: Callback) -> bool {
        let module = self.heap(plugin);
        match (
            self.function(plugin, module, a),
            self.function(plugin, module, b),
        ) {
            (Ok(a), Ok(b)) => a.ptr_eq(b),
            _ => false,
        }
    }

    fn function<'v>(
        &self,
        plugin: usize,
        module: &'v Module,
        callback: Callback,
    ) -> anyhow::Result<Value<'v>> {
        match callback {
            Callback::Frozen(index) => Ok(self.inner.plugins[plugin]
                .loaded
                .callback(index)?
                .to_value()),
            Callback::File(index) => file_callback(module, index),
        }
    }

    /// The ancestors of a node, innermost first.
    fn ancestors(origin: Origin<'_, 'a>, host: &dyn Host<'a>) -> Vec<NodeRef<'a>> {
        match origin {
            Origin::Current => host
                .ancestors()
                .into_iter()
                .skip(1)
                .map(to_node_ref)
                .collect(),
            Origin::Within(parents) => parents
                .iter()
                .copied()
                .chain(host.ancestors().into_iter().map(to_node_ref))
                .collect(),
        }
    }

    /// Finds the checks that select a node, innermost scope last.
    fn candidates(
        &self,
        node: NodeRef<'a>,
        kind: Kind,
        ancestors: &mut Option<Vec<NodeRef<'a>>>,
        origin: Origin<'_, 'a>,
        host: &dyn Host<'a>,
    ) -> SmallVec<[usize; 4]> {
        let state = self.state.borrow();
        let mut candidates: SmallVec<[usize; 4]> =
            state.by_kind[kind as usize].iter().copied().collect();
        if state.scoped_kinds.contains(kind) {
            if !state.scoped.is_empty() {
                let ancestors = ancestors.get_or_insert_with(|| Self::ancestors(origin, host));
                for ancestor in ancestors.iter().rev() {
                    if let Some(id) = state.ids.get(&ancestor.key())
                        && let Some(checks) = state.scoped.get(id)
                    {
                        candidates.extend(
                            checks
                                .iter()
                                .copied()
                                .filter(|&check| state.checks[check].selects(kind)),
                        );
                    }
                }
            }
            let within_scope = |index: usize| {
                let check = &state.checks[index];
                check.selects(kind)
                    && check.scope.is_some_and(|scope| {
                        let scope = state.nodes[scope as usize].node;
                        scope.range().contains_range(node.range()) && !scope.is(node)
                    })
            };
            if !state.scoped_within.is_empty() {
                let ancestors = ancestors.get_or_insert_with(|| Self::ancestors(origin, host));
                // The visited node itself can contain the scope, as a function contains its
                // parameters.
                let visited = match origin {
                    Origin::Current => Some(node),
                    Origin::Within(_) => None,
                };
                for ancestor in ancestors.iter().rev().chain(visited.as_ref()) {
                    if let Some(id) = state.ids.get(&ancestor.key())
                        && let Some(checks) = state.scoped_within.get(id)
                    {
                        candidates
                            .extend(checks.iter().copied().filter(|&check| within_scope(check)));
                    }
                }
            }
            candidates.extend(
                state
                    .scoped_by_range
                    .iter()
                    .copied()
                    .filter(|&check| within_scope(check)),
            );
        }
        // Only pass the node to each `foreach` callback once.
        let mut seen: SmallVec<[usize; 4]> = SmallVec::new();
        candidates.retain(|index| {
            let group = state.checks[*index].group;
            if state.failed[group] || seen.contains(&group) {
                false
            } else {
                seen.push(group);
                true
            }
        });
        candidates
    }

    fn dispatch(&self, node: NodeRef<'a>, origin: Origin<'_, 'a>, host: &dyn Host<'a>) {
        let Some(kind) = node.kind() else {
            return;
        };
        if !self.kinds.get().contains(kind) {
            return;
        }
        let mut ancestors = None;
        let candidates = self.candidates(node, kind, &mut ancestors, origin, host);
        if candidates.is_empty() {
            return;
        }
        let mut qualname: Option<Option<String>> = None;
        let mut id = None;
        for index in candidates {
            let (plugin, callback, group, passes) = {
                let state = self.state.borrow();
                let check = &state.checks[index];
                let passes = check.filters().iter().all(|filter| match filter {
                    Filter::Name(regex) => name_of(node).is_some_and(|name| regex.is_match(name)),
                    Filter::Qualname(patterns) => {
                        patterns.may_match(&self.roots.borrow())
                            && qualname
                                .get_or_insert_with(|| self.qualname_of(node, origin, host))
                                .as_deref()
                                .is_some_and(|name| patterns.matches(name))
                    }
                    Filter::Path { .. } | Filter::Text(_) => true,
                });
                (check.plugin, check.callback, check.group, passes)
            };
            if !passes {
                continue;
            }
            let id = *id.get_or_insert_with(|| {
                let ancestors = ancestors.get_or_insert_with(|| Self::ancestors(origin, host));
                let id = self.intern_with_ancestors(node, ancestors);
                if matches!(
                    kind,
                    Kind::Name | Kind::Attribute | Kind::Call | Kind::Alias
                ) {
                    let qualname = qualname
                        .take()
                        .unwrap_or_else(|| self.qualname_of(node, origin, host));
                    let _ = self.state.borrow().nodes[id as usize]
                        .qualname
                        .set(qualname);
                }
                id
            });
            self.invoke(
                host,
                plugin,
                callback,
                Arg::Node(id),
                node.range(),
                Some(group),
            );
        }
    }

    fn dispatch_import(&self, stmt: &'a Stmt, alias: &'a ast::Alias, host: &dyn Host<'a>) {
        let node = NodeRef::Ast(AnyNodeRef::from(alias));
        let ancestors: Vec<NodeRef<'a>> = host.ancestors().into_iter().map(to_node_ref).collect();
        let candidates = {
            let state = self.state.borrow();
            let ancestor_ids: Vec<u32> = ancestors
                .iter()
                .filter_map(|ancestor| state.ids.get(&ancestor.key()).copied())
                .collect();
            let (_, qualname) = self.import_names(AnyNodeRef::from(stmt), alias);
            let mut candidates = Vec::new();
            let mut seen = Vec::new();
            for &index in &state.imports {
                let check = &state.checks[index];
                if state.failed[check.group] || seen.contains(&check.group) {
                    continue;
                }
                let within = check.scope.is_none_or(|scope| {
                    ancestor_ids.contains(&scope)
                        || state.nodes[scope as usize]
                            .node
                            .range()
                            .contains_range(stmt.range())
                });
                let passes = check.filters().iter().all(|filter| match filter {
                    Filter::Name(regex) => regex.is_match(alias.name.as_str()),
                    Filter::Qualname(patterns) => patterns.matches(&qualname),
                    Filter::Path { .. } | Filter::Text(_) => true,
                });
                if within && passes {
                    seen.push(check.group);
                    candidates.push((check.plugin, check.callback, check.group));
                }
            }
            candidates
        };
        if candidates.is_empty() {
            return;
        }
        let id = self.intern_with_ancestors(node, &ancestors);
        for (plugin, callback, group) in candidates {
            self.invoke(
                host,
                plugin,
                callback,
                Arg::Node(id),
                alias.range(),
                Some(group),
            );
        }
    }

    /// Visits the children of a node whose kinds ruff doesn't visit on their own, such as the
    /// keyword arguments of a call or the parameters of a function.
    fn visit_children(
        &self,
        node: NodeRef<'a>,
        parents: &mut Vec<NodeRef<'a>>,
        host: &dyn Host<'a>,
    ) {
        if !self.child_checks.get() {
            return;
        }
        node.for_each_child(&mut |child| {
            if child.kind().is_some_and(is_child_kind) {
                // `parents` holds the nodes between `child` and the visited node, innermost first.
                self.dispatch(child, Origin::Within(parents), host);
                parents.insert(0, child);
                self.visit_children(child, parents, host);
                parents.remove(0);
            }
        });
    }

    /// Records the first segments of the qualified names that an import makes available.
    fn record_roots(&self, stmt: &Stmt) {
        let root = |name: &str| name.split('.').next().unwrap_or(name).to_string();
        match stmt {
            Stmt::Import(import) => {
                let mut roots = self.roots.borrow_mut();
                for alias in &import.names {
                    roots.insert(root(&alias.name));
                }
            }
            Stmt::ImportFrom(import) => {
                if let Some(module) = resolve_imported_module_path(
                    import.level,
                    import.module.as_deref(),
                    self.module_path.as_deref(),
                ) {
                    self.roots.borrow_mut().insert(root(&module));
                }
            }
            _ => {}
        }
    }

    /// The absolute module that an import imports from, and the qualified name of what it
    /// imports.
    pub(crate) fn import_names(&self, stmt: AnyNodeRef, alias: &ast::Alias) -> (String, String) {
        match stmt {
            AnyNodeRef::StmtImportFrom(import) => {
                let module = resolve_imported_module_path(
                    import.level,
                    import.module.as_deref(),
                    self.module_path.as_deref(),
                )
                .map(Cow::into_owned)
                .unwrap_or_else(|| {
                    // A relative import that we can't resolve, such as one in a script.
                    let dots = ".".repeat(import.level as usize);
                    format!("{dots}{}", import.module.as_deref().unwrap_or_default())
                });
                let qualname = if module.is_empty() || module.ends_with('.') {
                    format!("{module}{}", alias.name)
                } else {
                    format!("{module}.{}", alias.name)
                };
                (module, qualname)
            }
            _ => (alias.name.to_string(), alias.name.to_string()),
        }
    }

    /// Returns the id of a node, adding it to the table of nodes that plugins have seen.
    pub(crate) fn intern(&self, node: NodeRef<'a>) -> u32 {
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        let next = u32::try_from(state.nodes.len()).unwrap_or(u32::MAX);
        let id = *state.ids.entry(node.key()).or_insert(next);
        if id == next {
            state.nodes.push(Entry {
                node,
                parent: None,
                qualname: OnceCell::new(),
            });
        }
        id
    }

    /// Interns a node, given some of the nodes that contain it (innermost first), and records its
    /// chain of parents. The nodes between two of the given ones, which ruff's semantic model
    /// doesn't track (like the `keyword` between a call and the value of one of its keyword
    /// arguments), are found in the AST, so that a node's parent doesn't depend on how a plugin
    /// reached it.
    pub(crate) fn intern_with_ancestors(
        &self,
        node: NodeRef<'a>,
        ancestors: &[NodeRef<'a>],
    ) -> u32 {
        let id = self.intern(node);
        let (mut child, mut child_id) = (node, id);
        for &ancestor in ancestors {
            if self.state.borrow().nodes[child_id as usize]
                .parent
                .is_some()
            {
                // The rest of the chain is already known.
                break;
            }
            let ancestor_id = self.intern(ancestor);
            let mut parent = ancestor_id;
            for between in ancestor.path_to(child).unwrap_or_default() {
                let between_id = self.intern(between);
                self.set_parent(between_id, parent);
                parent = between_id;
            }
            self.set_parent(child_id, parent);
            (child, child_id) = (ancestor, ancestor_id);
        }
        id
    }

    fn set_parent(&self, id: u32, parent: u32) {
        let mut state = self.state.borrow_mut();
        if let Some(entry) = state.nodes.get_mut(id as usize)
            && entry.parent.is_none()
            && id != parent
        {
            entry.parent = Some(parent);
        }
    }

    /// Interns a node inside the node with id `container`, such as the value of one of its
    /// fields.
    pub(crate) fn intern_within(&self, node: NodeRef<'a>, container: u32) -> u32 {
        match self.entry(container) {
            Some((container, _)) => self.intern_with_ancestors(node, &[container]),
            None => self.intern(node),
        }
    }

    pub(crate) fn entry(&self, id: u32) -> Option<(NodeRef<'a>, Option<u32>)> {
        let state = self.state.borrow();
        state
            .nodes
            .get(id as usize)
            .map(|entry| (entry.node, entry.parent))
    }

    /// The qualified name of a name, attribute, call, or imported name.
    pub(crate) fn qualname(&self, id: u32, host: &dyn Host<'a>) -> Option<String> {
        let (node, parent) = self.entry(id)?;
        if let Some(qualname) = self.state.borrow().nodes[id as usize].qualname.get() {
            return qualname.clone();
        }
        match node {
            NodeRef::Ast(AnyNodeRef::Alias(alias)) => {
                let (NodeRef::Stmt(stmt), _) = self.entry(parent?)? else {
                    return None;
                };
                Some(self.import_names(stmt.into(), alias).1)
            }
            node => qualname_of_visited(node, host),
        }
    }

    /// The value of the node with id `id`, for a plugin callback.
    pub(crate) fn node_value(&self, id: u32, host: &dyn Host<'a>) -> Option<NodeValue> {
        let (node, _) = self.entry(id)?;
        let kind = node.kind()?;
        let qualname = if QUALIFIED.contains(&kind) || kind.is_expression() {
            self.qualname(id, host)
        } else {
            None
        };
        let data = kind
            .fields()
            .iter()
            .filter(|field| kind.is_data_field(field))
            .map(|field| (*field, Data::new(&crate::node::field(node, field))))
            .collect();
        Some(NodeValue {
            file: self.generation,
            id,
            kind,
            range: node.range(),
            source: self.source_file.get_or_init(|| host.source_file()).clone(),
            qualname,
            data,
        })
    }

    fn invoke(
        &self,
        host: &dyn Host<'a>,
        plugin: usize,
        callback: Callback,
        arg: Arg,
        range: TextRange,
        group: Option<usize>,
    ) {
        let module = self.heap(plugin);
        let active = Active {
            run: self,
            host,
            plugin,
        };
        let context = RunContext(&active);
        let result = (|| -> starlark::Result<()> {
            let mut eval = Evaluator::new(module);
            eval.extra = Some(&context);
            let function = self
                .function(plugin, module, callback)
                .map_err(starlark::Error::new_other)?;
            let heap = eval.heap();
            let args = match arg {
                Arg::Node(id) => match self.node_value(id, host) {
                    Some(node) => vec![node.alloc(heap)],
                    None => vec![Value::new_none()],
                },
                Arg::Module => vec![heap.alloc(ModuleValue {
                    file: self.generation,
                    path: self.paths[plugin].clone(),
                    name: self.module_name.clone(),
                })],
                Arg::None => Vec::new(),
            };
            eval.eval_function(function, &args, &[])?;
            Ok(())
        })();
        if let Err(err) = result {
            // Report each failing function once per file, and stop calling it, however many
            // times it was registered.
            let others: Vec<usize> = {
                let state = self.state.borrow();
                state
                    .checks
                    .iter()
                    .filter(|check| {
                        check.plugin == plugin
                            && !state.failed[check.group]
                            && self.same_function(plugin, check.callback, callback)
                    })
                    .map(|check| check.group)
                    .collect()
            };
            let mut state = self.state.borrow_mut();
            for group in group.into_iter().chain(others) {
                state.failed[group] = true;
            }
            state.failed_callbacks.push((plugin, callback));
            let location = err.span().map(|span| {
                (
                    PathBuf::from(span.filename()),
                    span.resolve_span().begin.line + 1,
                )
            });
            let message = err.without_diagnostic().to_string();
            let hint = hint(&message);
            // Starlark's traceback starts with a frame for the native code that called the
            // callback, which says nothing about the plugin.
            let mut lines: Vec<String> = format!("{:#}", err.into_anyhow())
                .lines()
                .filter(|line| line.trim() != "File <builtin>, in <module>")
                .map(str::to_string)
                .collect();
            if lines
                .first()
                .is_some_and(|line| line.starts_with("Traceback"))
                && lines.get(1).is_some_and(|line| line.starts_with("error:"))
            {
                lines.remove(0);
            }
            let details = lines.join("\n");
            state.errors.push(PluginError {
                plugin: &self.inner.plugins[plugin].spec.name,
                range,
                message,
                location,
                details,
                hint,
            });
        }
    }

    /// Records a diagnostic reported by the rule with index `rule` in plugin `plugin`.
    pub(crate) fn report(
        &self,
        plugin: usize,
        rule: usize,
        range: TextRange,
        message: String,
        edits: Vec<EditValue>,
        fix_title: Option<String>,
    ) -> anyhow::Result<()> {
        let rule = &self.inner.rules[self.inner.plugins[plugin].rule_offset + rule];
        // Fixes that delete statements from the same block are applied one at a time, so that
        // together they can't empty the block, as ruff's own fixes do.
        let isolation = edits
            .iter()
            .find_map(|edit| edit.deleted_statement)
            .map(|(parent, _)| IsolationLevel::Group(ISOLATION_GROUPS | parent));
        let mut edits = self.merge_edits(self.keep_blocks(edits))?.into_iter();
        let fix = edits.next().map(|first| {
            let fix = Fix::applicable_edits(first, edits, rule.fix_safety);
            match isolation {
                Some(isolation) => fix.isolate(isolation),
                None => fix,
            }
        });
        self.state.borrow_mut().findings.push(Finding {
            rule: &rule.info,
            message,
            range,
            fix,
            fix_title,
        });
        Ok(())
    }

    /// Replaces one of the deleted statements with `pass` if the edits delete every statement of
    /// a block.
    fn keep_blocks(&self, mut edits: Vec<EditValue>) -> Vec<EditValue> {
        let mut parents: Vec<u32> = edits
            .iter()
            .filter_map(|edit| edit.deleted_statement.map(|(parent, _)| parent))
            .collect();
        parents.sort_unstable();
        parents.dedup();
        for parent in parents {
            let Some((NodeRef::Stmt(parent_stmt), _)) = self.entry(parent) else {
                continue;
            };
            for block in crate::node::blocks(parent_stmt) {
                let is_deleted = |stmt: &Stmt| {
                    edits
                        .iter()
                        .any(|edit| edit.deleted_statement == Some((parent, stmt.range())))
                };
                if let [first, ..] = block
                    && block.iter().all(is_deleted)
                    && let Some(edit) = edits
                        .iter_mut()
                        .find(|edit| edit.deleted_statement == Some((parent, first.range())))
                {
                    edit.edit = Edit::range_replacement("pass".to_string(), first.range());
                }
            }
        }
        edits
    }

    /// Sorts the edits of a fix, merging duplicates and overlapping deletions (like those of two
    /// adjacent arguments, which share a comma). Other overlapping edits are an error, because
    /// ruff can't apply them.
    fn merge_edits(&self, edits: Vec<EditValue>) -> anyhow::Result<Vec<Edit>> {
        let source = self.source_file.get().map(SourceFile::source_text);
        // Edits that don't change anything would make ruff apply the fix again and again.
        let changes = |edit: &Edit| {
            let current = source.map(|source| &source[edit.range()]);
            match edit.content() {
                Some(content) => current != Some(content),
                None => !edit.range().is_empty(),
            }
        };
        let mut edits: Vec<Edit> = edits
            .into_iter()
            .map(|edit| edit.edit)
            .filter(changes)
            .collect();
        edits.sort_by_key(|edit| (edit.start(), edit.end()));
        let mut merged: Vec<Edit> = Vec::with_capacity(edits.len());
        for edit in edits {
            if let Some(last) = merged.last_mut()
                && edit.start() < last.end()
            {
                if *last == edit {
                    continue;
                }
                if last.is_deletion() && edit.is_deletion() {
                    *last = Edit::deletion(last.start(), last.end().max(edit.end()));
                    continue;
                }
                let text = |edit: &Edit| {
                    source.map_or_else(String::new, |source| {
                        format!(" `{}`", &source[edit.range()])
                    })
                };
                anyhow::bail!(
                    "The edits of a fix overlap, at{} and{}; combine them into one edit of the code around both",
                    text(last),
                    text(&edit)
                );
            }
            merged.push(edit);
        }
        Ok(merged)
    }

    /// Fails if it's too late to register checks.
    fn check_registering(&self) -> anyhow::Result<()> {
        if self.state.borrow().traversed {
            anyhow::bail!(
                "Checks can't be registered in `on_exit` or `comments()` callbacks, because ruff has already visited every node"
            );
        }
        Ok(())
    }

    /// Fails if it's too late to register `on_exit` callbacks.
    fn check_registering_exit(&self) -> anyhow::Result<()> {
        if self.state.borrow().finished {
            anyhow::bail!("`on_exit` callbacks can't be registered in `on_exit` callbacks");
        }
        Ok(())
    }
}

#[derive(Copy, Clone)]
enum Arg {
    Node(u32),
    Module,
    None,
}

impl<'a> FileRun<'a> {
    /// The qualified name of a node that ruff is visiting (or a child of it).
    fn qualname_of(
        &self,
        node: NodeRef<'a>,
        origin: Origin<'_, 'a>,
        host: &dyn Host<'a>,
    ) -> Option<String> {
        match node {
            NodeRef::Ast(AnyNodeRef::Alias(alias)) => {
                // An alias is a child of the import that ruff is visiting.
                let Origin::Within([]) = origin else {
                    return None;
                };
                let crate::Ancestor::Stmt(stmt) = *host.ancestors().first()? else {
                    return None;
                };
                Some(self.import_names(stmt.into(), alias).1)
            }
            node => qualname_of_visited(node, host),
        }
    }
}

/// How to fix the mistakes that Python programmers commonly make in plugins, given the error that
/// they cause.
fn hint(message: &str) -> Option<&'static str> {
    if message.starts_with("Object of type `function` has no attribute")
        || message.starts_with("Operation `(iter)` not supported on type `function`")
    {
        Some(
            "Fields that hold nodes are methods, so call them: `call.func().attr`, not `call.func.attr`",
        )
    } else if message.contains("has no attribute `kind`") {
        Some("A node's kind is its type: use `type(node)`, as in `type(node) == \"Call\"`")
    } else if message.starts_with("Operation `call()` not supported on type") {
        Some(
            "Fields that hold plain data, like names and operators, are attributes: `function.name`, not `function.name()`",
        )
    } else {
        None
    }
}

/// Resolves the qualified name of a node that ruff is visiting (or a child of it): the name a
/// name or attribute refers to, or the name of the function a call calls.
fn qualname_of_visited<'a>(node: NodeRef<'a>, host: &dyn Host<'a>) -> Option<String> {
    match node {
        NodeRef::Expr(Expr::Call(call)) => host.qualified_name(&call.func),
        NodeRef::Expr(expr @ (Expr::Name(_) | Expr::Attribute(_))) => host.qualified_name(expr),
        _ => None,
    }
}

fn file_callback(module: &Module, index: usize) -> anyhow::Result<Value<'_>> {
    module
        .extra_value()
        .and_then(ListRef::from_value)
        .and_then(|callbacks| callbacks.content().get(index).copied())
        .ok_or_else(|| anyhow::anyhow!("The file has no callback with index {index}"))
}

/// Registers a callback in the file's list of callbacks, returning its index.
pub(crate) fn push_callback<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    callback: Value<'v>,
) -> anyhow::Result<usize> {
    let callbacks = eval
        .module()
        .extra_value()
        .ok_or_else(|| anyhow::anyhow!("The module has no list of callbacks"))?;
    let append = callbacks
        .get_attr_error("append", eval.heap())
        .map_err(starlark::Error::into_anyhow)?;
    eval.eval_function(append, &[callback], &[])
        .map_err(starlark::Error::into_anyhow)?;
    ListRef::from_value(callbacks)
        .map(|callbacks| callbacks.len() - 1)
        .ok_or_else(|| anyhow::anyhow!("The module's callbacks aren't a list"))
}

/// Registers a check while linting a file, from `selector.foreach(callback)`.
pub(crate) fn register_check<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    selector: &Arc<Selector>,
    callback: Value<'v>,
) -> anyhow::Result<()> {
    with_run(run_context(eval)?, |active| {
        let run = active.run;
        run.check_registering()?;
        if selector
            .branches
            .iter()
            .any(|branch| matches!(branch.source, Source::Modules))
        {
            anyhow::bail!(
                "`modules()` checks must be registered when the plugin loads, at its top level"
            );
        }
        let scope = match selector.scope {
            Scope::Global => anyhow::bail!(
                "`{}` selects nodes in the whole file, so it must be registered when the plugin loads, at its top level. In a callback, select the nodes inside a node or module instead, like `node.{}`",
                selector.display,
                selector.display
            ),
            Scope::File(file) => {
                active.check_file(file)?;
                None
            }
            Scope::Node(file, id) => {
                active.check_file(file)?;
                Some(id)
            }
        };
        let index = push_callback(eval, callback)?;
        run.add_check(active.plugin, Callback::File(index), selector, scope);
        Ok(())
    })
}

/// Registers an `on_exit` callback for a node, or for the module if `node` is `None`.
pub(crate) fn register_exit<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    file: u64,
    node: Option<u32>,
    callback: Value<'v>,
) -> anyhow::Result<()> {
    with_run(run_context(eval)?, |active| {
        active.check_file(file)?;
        active.run.check_registering_exit()?;
        let index = push_callback(eval, callback)?;
        active.run.state.borrow_mut().exits.push(Exit {
            plugin: active.plugin,
            callback: index,
            scope: node,
        });
        Ok(())
    })
}

/// The file run and plugin that a callback is running for.
///
/// Starlark values can't borrow from the file being linted, so node values are ids, which the
/// methods of values resolve through the run. The run reaches them through the evaluator's
/// "extra" value, a [`RunContext`], which [`with_run`] reads.
pub(crate) struct Active<'a, 'r> {
    pub(crate) run: &'r FileRun<'a>,
    pub(crate) host: &'r dyn Host<'a>,
    pub(crate) plugin: usize,
}

impl Active<'_, '_> {
    /// Checks that a value belongs to the file that this run lints.
    pub(crate) fn check_file(&self, file: u64) -> anyhow::Result<()> {
        if file != self.run.generation {
            anyhow::bail!("This value belongs to another file, which ruff has finished linting");
        }
        Ok(())
    }
}

/// Erases the lifetime of the AST from an [`Active`], so that it can be the evaluator's "extra"
/// value, which can only have one lifetime. `visit` calls its argument for any lifetime of the
/// AST, so nothing that borrows from the AST can escape.
pub(crate) trait ActiveRun {
    fn visit(&self, f: &mut dyn for<'a, 'r> FnMut(&'r Active<'a, 'r>));
}

impl ActiveRun for Active<'_, '_> {
    fn visit(&self, f: &mut dyn for<'a, 'r> FnMut(&'r Active<'a, 'r>)) {
        f(self);
    }
}

/// The evaluator's "extra" value while a run calls one of the plugin's callbacks.
#[derive(ProvidesStaticType)]
pub(crate) struct RunContext<'r>(&'r dyn ActiveRun);

/// The context of the run that called the callback being evaluated. Fails if no run is active
/// (because the plugin is loading).
pub(crate) fn run_context<'a, 'e>(
    eval: &Evaluator<'_, 'a, 'e>,
) -> anyhow::Result<&'a RunContext<'e>> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<RunContext>())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "This is only available while ruff lints a file, not while the plugin loads"
            )
        })
}

/// Calls `f` with the run of a [`RunContext`].
pub(crate) fn with_run<R>(
    context: &RunContext<'_>,
    f: impl for<'a, 'r> FnOnce(&'r Active<'a, 'r>) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    let mut f = Some(f);
    let mut result = None;
    context.0.visit(&mut |active| {
        if let Some(f) = f.take() {
            result = Some(f(active));
        }
    });
    result.unwrap_or_else(|| Err(anyhow::anyhow!("The run didn't call back")))
}
