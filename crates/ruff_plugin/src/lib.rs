//! Lint rules written in Starlark.
//!
//! A plugin is a Starlark file that declares rules, and registers callbacks for the nodes that
//! it wants to check:
//!
//! ```python
//! TIMEOUT = rule("HTTP001", "http-request-without-timeout")
//!
//! def check(call):
//!     if not call.keyword("timeout"):
//!         TIMEOUT.report(call, "`{}` has no `timeout`, so it can hang forever".format(call.func().text))
//!
//! calls("requests.*").foreach(check)
//! ```
//!
//! Selectors like `calls("requests.*")` ([`selector`]) are evaluated natively during ruff's
//! traversal of each file ([`run`]), so callbacks only run for the nodes they're interested in.
//! Callbacks receive nodes ([`values`]) with the fields of Python's `ast` module ([`kind`],
//! [`node`]), and can keep state across nodes, and register more checks scoped to the nodes they
//! receive, much like ruff's own rules do.

// Starlark's `ProvidesStaticType` derive implements an unsafe trait.
#![expect(unsafe_code)]

use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ruff_cache::{CacheKey, CacheKeyHasher};
use ruff_diagnostics::{Applicability, Edit, Fix};
use ruff_python_ast::{Arguments, Expr, Stmt};
use ruff_source_file::SourceFile;
use ruff_text_size::TextRange;

use crate::load::LoadedPlugin;
pub use crate::run::FileRun;

mod edit;
mod kind;
mod load;
mod node;
mod re;
mod run;
mod selector;
#[cfg(test)]
mod tests;
mod values;

/// A plugin to load, as configured by the user.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginSpec {
    /// The name that the configuration gives the plugin.
    pub name: String,
    /// The path to the plugin's Starlark file.
    pub path: PathBuf,
    /// The directory that the paths that plugins see, and match with `filter_path`, are relative
    /// to.
    pub root: PathBuf,
    /// The plugin's options, in the order the configuration lists them.
    pub options: Vec<(String, OptionValue)>,
}

/// The value of a plugin option.
#[derive(Clone, Debug, PartialEq)]
pub enum OptionValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<OptionValue>),
    Table(Vec<(String, OptionValue)>),
}

impl fmt::Display for OptionValue {
    /// Formats the value as TOML would write it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OptionValue::Bool(value) => write!(f, "{value}"),
            OptionValue::Int(value) => write!(f, "{value}"),
            OptionValue::Float(value) => write!(f, "{value}"),
            OptionValue::String(value) => write!(f, "{value:?}"),
            OptionValue::Array(values) => {
                f.write_str("[")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{value}")?;
                }
                f.write_str("]")
            }
            OptionValue::Table(entries) => {
                f.write_str("{ ")?;
                for (index, (name, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name} = {value}")?;
                }
                f.write_str(" }")
            }
        }
    }
}

/// Information about a plugin rule.
#[derive(Debug)]
pub struct RuleInfo {
    /// The rule's code, like `ACME001`.
    pub code: String,
    /// The rule's name, like `no-print`. Names are interned for the lifetime of the process,
    /// because ruff's diagnostics refer to rules by `&'static str`.
    pub name: &'static str,
    /// The name of the plugin that defines the rule.
    pub plugin: String,
    /// A link to the rule's documentation.
    pub url: Option<String>,
}

/// A diagnostic reported by a plugin rule.
#[derive(Debug)]
pub struct Finding<'p> {
    pub rule: &'p RuleInfo,
    pub message: String,
    pub range: TextRange,
    pub fix: Option<Fix>,
    pub fix_title: Option<String>,
}

/// An error raised by one of a plugin's callbacks.
#[derive(Debug)]
pub struct PluginError<'p> {
    pub plugin: &'p str,
    /// The node that the callback was called for.
    pub range: TextRange,
    /// The error, without its traceback.
    pub message: String,
    /// The plugin file and line where the error was raised, if it was raised by Starlark code.
    pub location: Option<(PathBuf, usize)>,
    /// The error with its traceback, which points into the plugin.
    pub details: String,
    /// How to fix a common mistake that caused the error.
    pub hint: Option<&'static str>,
}

/// What the runtime needs to know about the file being linted, from ruff's semantic model.
pub trait Host<'a> {
    fn source(&self) -> &'a str;

    /// The file being linted, which the values of its nodes keep for their text and line.
    fn source_file(&self) -> SourceFile;

    /// The fully qualified name that a name or attribute expression refers to, with builtins
    /// qualified as `builtins.<name>`.
    fn qualified_name(&self, expr: &'a Expr) -> Option<String>;

    /// The statements and expressions that contain the node being visited, from the node itself
    /// outwards.
    fn ancestors(&self) -> Vec<Ancestor<'a>>;

    /// The file's line ending, for the lines that fixes insert.
    fn line_ending(&self) -> &'static str;

    /// Deletes a statement as ruff's own fixes do: with its line or its `;`, or by replacing it
    /// with `pass` if it's the only statement in a block of `parent`.
    fn delete_statement(&self, stmt: &'a Stmt, parent: Option<&'a Stmt>) -> Edit;

    /// Deletes the argument or keyword argument at `argument` from the arguments of a call or
    /// class definition, with its comma.
    fn delete_argument(
        &self,
        argument: TextRange,
        arguments: &'a Arguments,
    ) -> anyhow::Result<Edit>;
}

#[derive(Copy, Clone, Debug)]
pub enum Ancestor<'a> {
    Stmt(&'a Stmt),
    Expr(&'a Expr),
}

pub(crate) struct Plugin {
    spec: PluginSpec,
    loaded: Arc<LoadedPlugin>,
    /// The contents of the plugin file, for cache keys.
    source: String,
    /// The index of the plugin's first rule in [`Inner::rules`].
    rule_offset: usize,
}

pub(crate) struct CompiledRule {
    info: RuleInfo,
    fix_safety: Applicability,
}

pub(crate) struct Inner {
    plugins: Vec<Plugin>,
    rules: Vec<CompiledRule>,
}

/// The plugins of a configuration, loaded. Cheap to clone.
///
/// Loaded plugins are frozen: nothing can mutate them, not even their own Starlark functions,
/// which is what makes it safe to share them between threads and across panics.
#[derive(Clone, Default)]
pub struct PluginSet {
    inner: Option<Arc<AssertUnwindSafe<Inner>>>,
    /// Why the plugins failed to load, if they did. Settings keep the error rather than failing
    /// to resolve, so that commands that don't lint (like `ruff format`) still work, and an
    /// editor can lint without the plugins. `ruff check` fails with the error (see
    /// [`PluginSet::error`]).
    error: Option<Arc<str>>,
}

impl PluginSet {
    /// A set of plugins that failed to load, and so doesn't check anything.
    pub fn failed(error: &anyhow::Error) -> Self {
        Self {
            inner: None,
            error: Some(Arc::from(format!("{error:#}"))),
        }
    }

    /// Why the plugins failed to load, if they did.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn load(specs: impl IntoIterator<Item = PluginSpec>) -> anyhow::Result<Self> {
        let mut plugins = Vec::new();
        let mut rules: Vec<CompiledRule> = Vec::new();
        for spec in specs {
            let source = std::fs::read_to_string(&spec.path).map_err(|err| {
                anyhow::anyhow!(
                    "Failed to read plugin `{}` from `{}`: {err}",
                    spec.name,
                    spec.path.display()
                )
            })?;
            let loaded = load_cached(&spec, &source).map_err(|err| {
                anyhow::anyhow!("Failed to load plugin `{}`:\n{err:?}", spec.name)
            })?;
            let rule_offset = rules.len();
            for def in &loaded.rules {
                if let Some(existing) = rules.iter().find(|rule| rule.info.code == def.code) {
                    anyhow::bail!(
                        "Plugins `{}` and `{}` both define a rule with code `{}`",
                        existing.info.plugin,
                        spec.name,
                        def.code
                    );
                }
                rules.push(CompiledRule {
                    info: RuleInfo {
                        code: def.code.clone(),
                        name: intern(&def.name),
                        plugin: spec.name.clone(),
                        url: def.url.clone(),
                    },
                    fix_safety: def.fix_safety,
                });
            }
            plugins.push(Plugin {
                spec,
                loaded,
                source,
                rule_offset,
            });
        }
        if plugins.is_empty() {
            return Ok(Self::default());
        }
        Ok(Self {
            inner: Some(Arc::new(AssertUnwindSafe(Inner { plugins, rules }))),
            error: None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_none()
    }

    pub fn rules(&self) -> impl Iterator<Item = &RuleInfo> {
        self.inner
            .iter()
            .flat_map(|inner| inner.rules.iter().map(|rule| &rule.info))
    }

    /// Whether `code` is the code or name of one of the plugins' rules.
    pub fn has_rule(&self, code: &str) -> bool {
        self.rules()
            .any(|rule| rule.code == code || rule.name == code)
    }

    /// Prepares to lint the file at `path`, whose dotted module path is `module_path`. Returns
    /// `None` if no plugin checks the file.
    pub fn start_file(&self, path: &Path, module_path: Option<&[String]>) -> Option<FileRun<'_>> {
        let inner = self.inner.as_deref()?;
        FileRun::new(inner, path, module_path)
    }
}

impl fmt::Debug for PluginSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(error) = &self.error {
            return f.debug_tuple("Failed").field(error).finish();
        }
        f.debug_list()
            .entries(
                self.inner
                    .iter()
                    .flat_map(|inner| inner.plugins.iter().map(|plugin| &plugin.spec)),
            )
            .finish()
    }
}

impl fmt::Display for PluginSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(error) = &self.error {
            return write!(f, "(failed to load: {error})");
        }
        let Some(inner) = &self.inner else {
            return f.write_str("[]");
        };
        f.write_str("[\n")?;
        for plugin in &inner.plugins {
            write!(f, "\t{} = {}", plugin.spec.name, plugin.spec.path.display())?;
            if !plugin.spec.options.is_empty() {
                f.write_str(" with options { ")?;
                for (index, (name, value)) in plugin.spec.options.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name} = {value}")?;
                }
                f.write_str(" }")?;
            }
            f.write_str(",\n")?;
        }
        f.write_str("]")
    }
}

impl CacheKey for PluginSet {
    fn cache_key(&self, state: &mut CacheKeyHasher) {
        self.error.as_deref().cache_key(state);
        let Some(inner) = &self.inner else {
            0usize.cache_key(state);
            return;
        };
        inner.plugins.len().cache_key(state);
        for plugin in &inner.plugins {
            plugin.spec.name.cache_key(state);
            plugin.spec.path.cache_key(state);
            plugin.source.cache_key(state);
            // Option values contain floats, which `CacheKey` doesn't support; their debug
            // representation identifies them just as well.
            format!("{:?}", plugin.spec.options).hash(&mut *state);
        }
    }
}

/// Loads a plugin, reusing the result of loading the same file with the same options earlier.
///
/// Ruff often resolves the same configuration more than once (for example, when a project's
/// root configuration is found both directly and while walking the project), and loading a
/// plugin runs its Starlark code. The cache holds one entry per plugin file, so editing a plugin
/// (in an editor session, say) replaces its entry rather than growing the cache.
fn load_cached(spec: &PluginSpec, source: &str) -> anyhow::Result<Arc<LoadedPlugin>> {
    type Entry = (String, PluginSpec, Arc<LoadedPlugin>);
    static CACHE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

    let lock = || CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, _, loaded)) = lock()
        .iter()
        .find(|(cached, cached_spec, _)| cached == source && cached_spec == spec)
    {
        return Ok(loaded.clone());
    }
    // Loading runs the plugin's code, which can take arbitrarily long, so it doesn't hold the
    // lock: other plugins can load meanwhile. Two threads may load the same plugin at once,
    // which only costs the time of loading it twice.
    let loaded = Arc::new(load::load(spec, source)?);
    let mut cache = lock();
    cache.retain(|(_, cached_spec, _)| cached_spec.path != spec.path);
    cache.push((source.to_string(), spec.clone(), loaded.clone()));
    Ok(loaded)
}

/// Interns a rule name, leaking each distinct name once.
fn intern(name: &str) -> &'static str {
    static NAMES: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut names = NAMES.lock().unwrap_or_else(PoisonError::into_inner);
    let names = names.get_or_insert_with(HashSet::new);
    if let Some(name) = names.get(name) {
        return name;
    }
    let name: &'static str = Box::leak(name.to_string().into_boxed_str());
    names.insert(name);
    name
}

/// Formats a "did you mean" suggestion for `name` among `candidates`, if one is close enough.
pub(crate) fn did_you_mean<'c>(
    name: &str,
    candidates: impl IntoIterator<Item = &'c str>,
) -> String {
    let best = candidates
        .into_iter()
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(distance, candidate)| *distance <= (name.len().max(candidate.len()) / 3).max(1))
        .min_by_key(|(distance, _)| *distance);
    match best {
        Some((_, candidate)) => format!(" (did you mean `{candidate}`?)"),
        None => String::new(),
    }
}

/// The edit distance between two strings, counting insertions, deletions, substitutions, and
/// transpositions of adjacent characters (as in `fucn` for `func`).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    // `distances[i][j]` is the distance between the first `i` characters of `a` and the first
    // `j` characters of `b`.
    let mut distances = vec![vec![0; b.len() + 1]; a.len() + 1];
    for (i, row) in distances.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, distance) in distances[0].iter_mut().enumerate() {
        *distance = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let substitution = distances[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]);
            let mut distance = substitution
                .min(distances[i - 1][j] + 1)
                .min(distances[i][j - 1] + 1);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                distance = distance.min(distances[i - 2][j - 2] + 1);
            }
            distances[i][j] = distance;
        }
    }
    distances[a.len()][b.len()]
}
