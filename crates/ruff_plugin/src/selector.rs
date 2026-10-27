//! Selectors: the natively evaluated part of a plugin, which decides what its callbacks see.
//!
//! A selector names the nodes to visit, like `calls("requests.*")`, and narrows them with
//! filters, like `.filter_path("src/**")`. Its `foreach(callback)` registers the callback, which
//! ruff then calls for each matching node while it traverses a file. Selectors combine with `|`.
//!
//! Selectors made at the top level of a plugin apply to every file. Selectors made from a module
//! or node, like `function.calls(...)`, only apply inside it: their callbacks run for the
//! matching nodes that ruff visits after the callback that registered them, which (because ruff
//! visits each node before its children) includes everything inside the node.

use std::fmt;
use std::sync::Arc;

use allocative::Allocative;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use rustc_hash::FxHashSet;
use starlark::any::ProvidesStaticType;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::none::NoneType;
use starlark::values::tuple::UnpackTuple;
use starlark::values::{Heap, NoSerialize, StarlarkValue, Value, ValueLike, starlark_value};

use crate::kind::{Kind, KindSet};

/// What a selector's branch visits.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    Nodes(KindSet),
    /// Imported names: one `alias` per name.
    Imports,
    Comments,
    /// The file itself, once, before ruff visits any of its nodes.
    Modules,
}

#[derive(Clone, Debug)]
pub(crate) enum Filter {
    /// Keeps files whose path (relative to the plugin's root) matches, or doesn't match.
    Path { globs: GlobSet, exclude: bool },
    /// Keeps nodes whose name (see [`crate::values::name_of`]) matches.
    Name(Regex),
    /// Keeps nodes whose qualified name (the `qualname` of their value) matches.
    Qualname(Patterns),
    /// Keeps comments whose text matches any of the expressions.
    Text(Vec<Regex>),
}

/// One alternative of a selector: a source, and the filters that its nodes must pass.
#[derive(Clone, Debug)]
pub(crate) struct Branch {
    pub(crate) source: Source,
    pub(crate) filters: Vec<Filter>,
}

/// Where a selector applies.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Every file: the selector was made while the plugin loaded.
    Global,
    /// The rest of the file being linted, identified by its run's generation.
    File(u64),
    /// The node with this id, in the file being linted.
    Node(u64, u32),
}

#[derive(Clone, Debug)]
pub(crate) struct Selector {
    pub(crate) branches: Vec<Branch>,
    pub(crate) scope: Scope,
    /// The Starlark expression that built the selector, for messages.
    pub(crate) display: String,
}

impl Selector {
    pub(crate) fn new(source: Source, scope: Scope, display: String) -> Self {
        Self {
            branches: vec![Branch {
                source,
                filters: Vec::new(),
            }],
            scope,
            display,
        }
    }

    fn with_filter(&self, filter: &Filter, display: &str) -> Self {
        let mut selector = self.clone();
        for branch in &mut selector.branches {
            branch.filters.push(filter.clone());
        }
        // `a | b` displays as `(a | b)` when filtered, so that the filter visibly applies to both.
        if self.branches.len() > 1 {
            selector.display = format!("({})", self.display);
        }
        selector.display.push_str(display);
        selector
    }

    /// Checks that every branch has nodes that a filter can apply to.
    fn check_filter(&self, method: &str, supports: impl Fn(&Source) -> bool) -> anyhow::Result<()> {
        if let Some(branch) = self
            .branches
            .iter()
            .find(|branch| !supports(&branch.source))
        {
            anyhow::bail!(
                "`{method}` doesn't apply to {}; it filters {}",
                describe(&branch.source),
                match method {
                    "filter_name" => {
                        "definitions, names, attributes, parameters, keyword arguments, and imported names"
                    }
                    _ => "names, attributes, calls, imported names, and modules",
                }
            );
        }
        Ok(())
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display)
    }
}

fn describe(source: &Source) -> String {
    match source {
        Source::Nodes(kinds) => format!("`{kinds}` nodes"),
        Source::Imports => "imports".to_string(),
        Source::Comments => "comments".to_string(),
        Source::Modules => "modules".to_string(),
    }
}

/// The kinds of nodes that `filter_name` applies to.
pub(crate) const NAMED: &[Kind] = &[
    Kind::FunctionDef,
    Kind::AsyncFunctionDef,
    Kind::ClassDef,
    Kind::Name,
    Kind::Attribute,
    Kind::Arg,
    Kind::Keyword,
    Kind::Alias,
    Kind::ExceptHandler,
];

/// The kinds of nodes that `filter_qualname` applies to.
pub(crate) const QUALIFIED: &[Kind] = &[Kind::Name, Kind::Attribute, Kind::Call, Kind::Alias];

fn has_any(kinds: KindSet, of: &[Kind]) -> bool {
    of.iter().any(|kind| kinds.contains(*kind))
}

/// Qualified names, like `requests.get`, or patterns that match everything in a module, like
/// `requests.*`.
#[derive(Clone, Debug)]
pub(crate) struct Patterns {
    exact: Vec<String>,
    /// Module prefixes from patterns like `requests.*`, including the trailing `.`.
    prefixes: Vec<String>,
    /// The first segments of the patterns, like `requests`.
    roots: Vec<String>,
}

impl Patterns {
    pub(crate) fn parse<'s>(patterns: impl IntoIterator<Item = &'s str>) -> anyhow::Result<Self> {
        let mut exact = Vec::new();
        let mut prefixes = Vec::new();
        for pattern in patterns {
            let (name, is_prefix) = match pattern.strip_suffix(".*") {
                Some(module) => (module, true),
                None => (pattern, false),
            };
            if name.is_empty()
                || !name
                    .split('.')
                    .all(ruff_python_stdlib::identifiers::is_identifier)
            {
                anyhow::bail!(
                    "`{pattern}` is not a qualified name like `requests.get`, or a pattern like `requests.*`"
                );
            }
            if is_prefix {
                prefixes.push(format!("{name}."));
            } else {
                if !name.contains('.')
                    && ruff_python_stdlib::builtins::is_python_builtin(name, 14, false)
                {
                    anyhow::bail!(
                        "`{name}` is not a qualified name; use `builtins.{name}` to match the builtin"
                    );
                }
                exact.push(name.to_string());
            }
        }
        let mut roots: Vec<String> = exact
            .iter()
            .chain(&prefixes)
            .filter_map(|pattern| pattern.split('.').next())
            .map(str::to_string)
            .collect();
        roots.sort_unstable();
        roots.dedup();
        Ok(Patterns {
            exact,
            prefixes,
            roots,
        })
    }

    /// Whether a name whose first segment is one of `roots` could match.
    pub(crate) fn may_match(&self, roots: &FxHashSet<String>) -> bool {
        self.roots.iter().any(|root| roots.contains(root))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.prefixes.is_empty()
    }

    pub(crate) fn matches(&self, qualified_name: &str) -> bool {
        self.exact.iter().any(|name| name == qualified_name)
            || self
                .prefixes
                .iter()
                .any(|prefix| qualified_name.starts_with(prefix.as_str()))
    }
}

pub(crate) fn kinds(names: &[&str]) -> anyhow::Result<KindSet> {
    if names.is_empty() {
        anyhow::bail!(
            "Expected at least one node kind, like `\"Call\"`, or `\"stmt\"` or `\"expr\"`"
        );
    }
    KindSet::parse(names.iter().copied())
}

fn quoted(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the selectors that `nodes(...)`, `calls(...)`, and so on return, both as globals and as
/// methods of modules and nodes.
pub(crate) fn nodes(names: &[&str], scope: Scope, prefix: &str) -> anyhow::Result<Selector> {
    Ok(Selector::new(
        Source::Nodes(kinds(names)?),
        scope,
        format!("{prefix}nodes({})", quoted(names)),
    ))
}

pub(crate) fn calls(patterns: &[&str], scope: Scope, prefix: &str) -> anyhow::Result<Selector> {
    qualified(KindSet::of(&[Kind::Call]), "calls", patterns, scope, prefix)
}

pub(crate) fn references(
    patterns: &[&str],
    scope: Scope,
    prefix: &str,
) -> anyhow::Result<Selector> {
    qualified(
        KindSet::of(&[Kind::Name, Kind::Attribute]),
        "references",
        patterns,
        scope,
        prefix,
    )
}

fn qualified(
    kinds: KindSet,
    function: &str,
    patterns: &[&str],
    scope: Scope,
    prefix: &str,
) -> anyhow::Result<Selector> {
    let mut selector = Selector::new(
        Source::Nodes(kinds),
        scope,
        format!("{prefix}{function}({})", quoted(patterns)),
    );
    let patterns = Patterns::parse(patterns.iter().copied())?;
    if !patterns.is_empty() {
        selector.branches[0]
            .filters
            .push(Filter::Qualname(patterns));
    }
    Ok(selector)
}

pub(crate) fn imports(patterns: &[&str], scope: Scope, prefix: &str) -> anyhow::Result<Selector> {
    let mut selector = Selector::new(
        Source::Imports,
        scope,
        format!("{prefix}imports({})", quoted(patterns)),
    );
    let patterns = Patterns::parse(patterns.iter().copied())?;
    if !patterns.is_empty() {
        selector.branches[0]
            .filters
            .push(Filter::Qualname(patterns));
    }
    Ok(selector)
}

pub(crate) fn comments(patterns: &[&str], scope: Scope, prefix: &str) -> anyhow::Result<Selector> {
    let mut selector = Selector::new(
        Source::Comments,
        scope,
        format!("{prefix}comments({})", quoted(patterns)),
    );
    if !patterns.is_empty() {
        let regexes = patterns
            .iter()
            .map(|pattern| crate::re::compile(pattern).map(|regex| Regex::clone(&regex)))
            .collect::<anyhow::Result<_>>()?;
        selector.branches[0].filters.push(Filter::Text(regexes));
    }
    Ok(selector)
}

fn globs(patterns: &[&str]) -> anyhow::Result<GlobSet> {
    if patterns.is_empty() {
        anyhow::bail!("Expected at least one path pattern, like `\"src/**\"`");
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            Glob::new(pattern)
                .map_err(|err| anyhow::anyhow!("Invalid path pattern `{pattern}`: {err}"))?,
        );
    }
    Ok(builder.build()?)
}

/// The Starlark value of a [`Selector`].
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct SelectorValue(#[allocative(skip)] pub(crate) Arc<Selector>);

starlark_simple_value!(SelectorValue);

impl SelectorValue {
    pub(crate) fn new(selector: Selector) -> Self {
        Self(Arc::new(selector))
    }
}

impl fmt::Display for SelectorValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[starlark_value(type = "Selector")]
impl<'v> StarlarkValue<'v> for SelectorValue {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new();
        METHODS.methods(selector_methods)
    }

    fn bit_or(&self, other: Value<'v>, heap: &'v Heap) -> starlark::Result<Value<'v>> {
        let Some(other) = other.downcast_ref::<SelectorValue>() else {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "Selectors can only be combined with other selectors, not a `{}`",
                other.get_type()
            )));
        };
        if self.0.scope != other.0.scope {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "`{}` and `{}` apply to different scopes, so they can't be combined",
                self.0,
                other.0
            )));
        }
        let mut selector = (*self.0).clone();
        selector.branches.extend(other.0.branches.iter().cloned());
        selector.display = format!("{} | {}", self.0, other.0);
        Ok(heap.alloc(SelectorValue::new(selector)))
    }
}

#[starlark_module]
fn selector_methods(builder: &mut MethodsBuilder) {
    /// Keeps the nodes of files whose path, relative to the project root, matches any of the
    /// glob patterns.
    fn filter_path<'v>(
        this: &SelectorValue,
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        let filter = Filter::Path {
            globs: globs(&patterns.items)?,
            exclude: false,
        };
        let display = format!(".filter_path({})", quoted(&patterns.items));
        Ok(SelectorValue::new(this.0.with_filter(&filter, &display)))
    }

    /// Drops the nodes of files whose path, relative to the project root, matches any of the glob
    /// patterns.
    fn exclude_path<'v>(
        this: &SelectorValue,
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        let filter = Filter::Path {
            globs: globs(&patterns.items)?,
            exclude: true,
        };
        let display = format!(".exclude_path({})", quoted(&patterns.items));
        Ok(SelectorValue::new(this.0.with_filter(&filter, &display)))
    }

    /// Keeps the nodes whose name matches the regular expression (anywhere, unless anchored with
    /// `^` and `$`). The name of a definition is its `name`, of a `Name` its `id`, of an
    /// `Attribute` its `attr`, of an `arg` or `keyword` its `arg`, and of an `alias` its `name`.
    fn filter_name(this: &SelectorValue, pattern: &str) -> anyhow::Result<SelectorValue> {
        this.0.check_filter("filter_name", |source| match source {
            Source::Nodes(kinds) => has_any(*kinds, NAMED),
            Source::Imports | Source::Modules => true,
            Source::Comments => false,
        })?;
        // Selectors are shared between threads, so they keep their own copy of the expression.
        let filter = Filter::Name(Regex::clone(&*crate::re::compile(pattern)?));
        let display = format!(".filter_name({pattern:?})");
        Ok(SelectorValue::new(this.0.with_filter(&filter, &display)))
    }

    /// Keeps the nodes whose qualified name matches any of the patterns, like `requests.get` or
    /// `requests.*`. A call's qualified name is that of the function it calls.
    fn filter_qualname<'v>(
        this: &SelectorValue,
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        this.0
            .check_filter("filter_qualname", |source| match source {
                Source::Nodes(kinds) => has_any(*kinds, QUALIFIED),
                Source::Imports | Source::Modules => true,
                Source::Comments => false,
            })?;
        let parsed = Patterns::parse(patterns.items.iter().copied())?;
        if parsed.is_empty() {
            anyhow::bail!("Expected at least one qualified name, like `\"requests.get\"`");
        }
        let display = format!(".filter_qualname({})", quoted(&patterns.items));
        Ok(SelectorValue::new(
            this.0.with_filter(&Filter::Qualname(parsed), &display),
        ))
    }

    /// Calls `callback` with each matching node (or module) as ruff visits it.
    fn foreach<'v>(
        this: &SelectorValue,
        callback: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        // Starlark has several types of callable values, so this only rejects common mistakes.
        if matches!(
            callback.get_type(),
            "NoneType" | "string" | "int" | "float" | "bool" | "list" | "dict" | "tuple"
        ) {
            anyhow::bail!(
                "`foreach` expects a function, but got a `{}`",
                callback.get_type()
            );
        }
        if crate::load::register_check(eval, this.0.clone(), callback)? {
            return Ok(NoneType);
        }
        crate::run::register_check(eval, &this.0, callback)?;
        Ok(NoneType)
    }
}
