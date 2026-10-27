//! Loading plugins: evaluating a plugin's Starlark file to collect its rules and checks.

// The functions of a `#[starlark_module]` must return `Result`s.
#![expect(clippy::unnecessary_wraps)]

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock};

use ruff_diagnostics::Applicability;
use starlark::any::ProvidesStaticType;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::AllocDict;
use starlark::values::list::{AllocList, ListRef};
use starlark::values::none::NoneType;
use starlark::values::tuple::UnpackTuple;
use starlark::values::{FrozenValue, Heap, Value};

use crate::edit::{EditOp, EditValue};
use crate::re::ReModule;
use crate::run::{push_callback, run_context};
use crate::selector::{self, Scope, Selector, SelectorValue, Source};
use crate::values::{RuleValue, make_edit};
use crate::{OptionValue, PluginSpec, did_you_mean};

/// A rule, as declared by `rule(...)`.
#[derive(Clone, Debug)]
pub(crate) struct RuleDef {
    pub(crate) code: String,
    pub(crate) name: String,
    pub(crate) fix_safety: Applicability,
    pub(crate) url: Option<String>,
}

/// A check registered while the plugin loaded, by `selector.foreach(callback)`.
pub(crate) struct LoadCheck {
    pub(crate) selector: Arc<Selector>,
    /// The index of the callback in the frozen module's list of callbacks.
    pub(crate) callback: usize,
}

/// A plugin whose rules and checks have been collected and whose module has been frozen.
pub(crate) struct LoadedPlugin {
    pub(crate) rules: Vec<RuleDef>,
    pub(crate) checks: Vec<LoadCheck>,
    /// The frozen module. Its "extra value" is the list of callbacks that `checks` refer to.
    module: FrozenModule,
}

impl LoadedPlugin {
    pub(crate) fn callback(&self, index: usize) -> anyhow::Result<FrozenValue> {
        self.module
            .extra_value()
            .and_then(|callbacks| ListRef::from_value(callbacks.to_value()))
            .and_then(|callbacks| callbacks.content().get(index).copied())
            .and_then(Value::unpack_frozen)
            .ok_or_else(|| anyhow::anyhow!("The plugin has no callback with index {index}"))
    }
}

/// State that the plugin's calls to `rule(...)`, `option(...)`, and `foreach(...)` accumulate
/// while the plugin loads.
///
/// Callbacks are kept in the module's "extra value" instead, because values on the module's
/// heap can't be stored outside of it.
#[derive(ProvidesStaticType)]
struct LoadContext {
    spec: PluginSpec,
    rules: RefCell<Vec<RuleDef>>,
    checks: RefCell<Vec<LoadCheck>>,
    /// The names of the options that the plugin declared, with their help text.
    declared_options: RefCell<Vec<(String, Option<String>)>>,
}

impl LoadContext {
    fn get<'a>(eval: &Evaluator<'_, 'a, '_>) -> Option<&'a LoadContext> {
        eval.extra
            .and_then(|extra| extra.downcast_ref::<LoadContext>())
    }

    fn require<'a>(eval: &Evaluator<'_, 'a, '_>, what: &str) -> anyhow::Result<&'a LoadContext> {
        Self::get(eval).ok_or_else(|| {
            anyhow::anyhow!("{what} only works while the plugin loads, at its top level")
        })
    }
}

/// Registers a check if the plugin is loading. Returns `false` if it isn't, in which case the
/// check is registered for the file being linted instead.
pub(crate) fn register_check<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    selector: Arc<Selector>,
    callback: Value<'v>,
) -> anyhow::Result<bool> {
    let Some(context) = LoadContext::get(eval) else {
        return Ok(false);
    };
    debug_assert_eq!(selector.scope, Scope::Global);
    let callback = push_callback(eval, callback)?;
    context
        .checks
        .borrow_mut()
        .push(LoadCheck { selector, callback });
    Ok(true)
}

static GLOBALS: LazyLock<Globals> = LazyLock::new(|| {
    GlobalsBuilder::extended_by(&[
        LibraryExtension::StructType,
        LibraryExtension::Typing,
        LibraryExtension::Print,
        LibraryExtension::Json,
        LibraryExtension::SetType,
        LibraryExtension::Map,
        LibraryExtension::Filter,
    ])
    .with(plugin_globals)
    .with(|builder| {
        builder.set("re", ReModule);
    })
    .build()
});

/// The Starlark dialect of plugins: the extended dialect (with nested functions, lambdas, and
/// top-level `if` and `for`), and f-strings (of variables, like `f"{name}"`), which Python
/// programmers expect.
const DIALECT: Dialect = Dialect {
    enable_f_strings: true,
    ..Dialect::Extended
};

/// Fails `load(...)` statements with an error that explains why.
struct NoLoader;

impl FileLoader for NoLoader {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        Err(starlark::Error::new_other(anyhow::anyhow!(
            "Plugins can't load other files (`{path}`) yet; keep each plugin in one file"
        )))
    }
}

/// Evaluates a plugin's source, collecting its rules and checks.
pub(crate) fn load(spec: &PluginSpec, source: &str) -> anyhow::Result<LoadedPlugin> {
    let filename = spec.path.display().to_string();
    let ast = AstModule::parse(&filename, source.to_string(), &DIALECT)
        .map_err(starlark::Error::into_anyhow)?;

    let module = Module::new();
    module.set_extra_value(module.heap().alloc(AllocList::EMPTY));
    let context = LoadContext {
        spec: spec.clone(),
        rules: RefCell::new(Vec::new()),
        checks: RefCell::new(Vec::new()),
        declared_options: RefCell::new(Vec::new()),
    };
    {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&context);
        eval.set_loader(&NoLoader);
        eval.eval_module(ast, &GLOBALS)
            .map_err(starlark::Error::into_anyhow)?;
    }

    let declared = context.declared_options.into_inner();
    for (name, _) in &spec.options {
        if declared.iter().any(|(declared, _)| declared == name) {
            continue;
        }
        if declared.is_empty() {
            anyhow::bail!(
                "Plugin `{}` has no option named `{name}`; it doesn't have any options",
                spec.name
            );
        }
        let mut message = format!(
            "Plugin `{}` has no option named `{name}`{}\n\nIts options are:",
            spec.name,
            did_you_mean(name, declared.iter().map(|(name, _)| name.as_str()))
        );
        for (name, help) in &declared {
            let _ = match help {
                Some(help) => write!(message, "\n  {name}: {help}"),
                None => write!(message, "\n  {name}"),
            };
        }
        anyhow::bail!(message);
    }

    let rules = context.rules.into_inner();
    if rules.is_empty() {
        anyhow::bail!(
            "Plugin `{}` doesn't define any rules; define one with `rule(\"CODE001\", \"rule-name\")`",
            spec.name
        );
    }
    let checks = context.checks.into_inner();
    if checks.is_empty() {
        anyhow::bail!(
            "Plugin `{}` doesn't check anything; register a check with `nodes(...).foreach(callback)`",
            spec.name
        );
    }

    let module = module
        .freeze()
        .map_err(|err| anyhow::anyhow!("Failed to freeze plugin `{}`: {err:?}", spec.name))?;
    Ok(LoadedPlugin {
        rules,
        checks,
        module,
    })
}

fn option_to_value<'v>(value: &OptionValue, heap: &'v Heap) -> Value<'v> {
    match value {
        OptionValue::Bool(value) => Value::new_bool(*value),
        OptionValue::Int(value) => heap.alloc(*value),
        OptionValue::Float(value) => heap.alloc(*value),
        OptionValue::String(value) => heap.alloc(value.as_str()),
        OptionValue::Array(values) => heap.alloc(AllocList(
            values.iter().map(|value| option_to_value(value, heap)),
        )),
        OptionValue::Table(entries) => heap.alloc(AllocDict(
            entries
                .iter()
                .map(|(key, value)| (key.as_str(), option_to_value(value, heap))),
        )),
    }
}

fn type_name(value: Value) -> &'static str {
    // Configuration can't express `None`, so an option without a default accepts anything.
    match value.get_type() {
        "int" | "float" => "number",
        "NoneType" => "any",
        other => other,
    }
}

fn is_rule_code(code: &str) -> bool {
    let digits = code.trim_start_matches(|c: char| c.is_ascii_uppercase());
    digits.len() < code.len() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn is_rule_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[starlark_module]
fn plugin_globals(builder: &mut GlobalsBuilder) {
    /// Selects the nodes of the given kinds, named after the classes in Python's `ast` module
    /// (`"Call"`, `"FunctionDef"`, ...), or `"stmt"` and `"expr"` for all statements or
    /// expressions.
    fn nodes<'v>(#[starlark(args)] kinds: UnpackTuple<&'v str>) -> anyhow::Result<SelectorValue> {
        Ok(SelectorValue::new(selector::nodes(
            &kinds.items,
            Scope::Global,
            "",
        )?))
    }

    /// Selects calls, optionally only those of functions with these qualified names, like
    /// `requests.get`, or in these modules, like `requests.*`. Names are resolved through
    /// imports and aliases: `from requests import get as fetch; fetch(url)` calls
    /// `requests.get`. Builtins are qualified as `builtins.print`.
    fn calls<'v>(
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        Ok(SelectorValue::new(selector::calls(
            &patterns.items,
            Scope::Global,
            "",
        )?))
    }

    /// Selects references to names: names and attributes, optionally only those that refer to
    /// these qualified names (as for `calls`).
    fn references<'v>(
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        Ok(SelectorValue::new(selector::references(
            &patterns.items,
            Scope::Global,
            "",
        )?))
    }

    /// Selects imported names, as `alias` nodes whose `qualname` is the absolute name of what's
    /// imported, optionally only those whose `qualname` matches the patterns.
    fn imports<'v>(
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        Ok(SelectorValue::new(selector::imports(
            &patterns.items,
            Scope::Global,
            "",
        )?))
    }

    /// Selects comments, as `Comment` nodes whose `text` includes the `#`, optionally only those
    /// that match any of the regular expressions.
    fn comments<'v>(
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        Ok(SelectorValue::new(selector::comments(
            &patterns.items,
            Scope::Global,
            "",
        )?))
    }

    /// Selects the file being linted, as a `Module` with the attributes `path` (relative to the
    /// project root) and `name` (the dotted module name, or `None` outside of a package). Its
    /// callbacks run before ruff visits any of the file's nodes, so they can set up state for
    /// the checks that they register.
    fn modules() -> anyhow::Result<SelectorValue> {
        Ok(SelectorValue::new(Selector::new(
            Source::Modules,
            Scope::Global,
            "modules()".to_string(),
        )))
    }

    /// Replaces `node` (or a span) with `text`.
    fn replace<'v>(
        node: Value<'v>,
        text: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<EditValue> {
        make_edit(EditOp::Replace, node, text, run_context(eval)?)
    }

    /// Deletes `node` (or a span). Statements are deleted with their line, and items of
    /// comma-separated lists (like arguments) with their comma.
    fn delete<'v>(node: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<EditValue> {
        make_edit(EditOp::Delete, node, "", run_context(eval)?)
    }

    /// Inserts `text` before `node`. Before a statement, this inserts a line.
    fn insert_before<'v>(
        node: Value<'v>,
        text: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<EditValue> {
        make_edit(EditOp::InsertBefore, node, text, run_context(eval)?)
    }

    /// Inserts `text` after `node`. After a statement, this inserts a line.
    fn insert_after<'v>(
        node: Value<'v>,
        text: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<EditValue> {
        make_edit(EditOp::InsertAfter, node, text, run_context(eval)?)
    }

    /// Reads the plugin option `name` from the configuration, or returns `default`. `help`
    /// describes the option in error messages about the plugin's configuration.
    fn option<'v>(
        name: &str,
        #[starlark(default = NoneType)] default: Value<'v>,
        #[starlark(require = named)] help: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let context = LoadContext::require(eval, "`option`")?;
        let mut declared = context.declared_options.borrow_mut();
        if declared.iter().any(|(declared, _)| declared == name) {
            anyhow::bail!("The option `{name}` is declared twice");
        }
        declared.push((name.to_string(), help.map(str::to_string)));
        let Some((_, value)) = context.spec.options.iter().find(|(key, _)| key == name) else {
            return Ok(default);
        };
        let value = option_to_value(value, eval.heap());
        let (expected, actual) = (type_name(default), type_name(value));
        if expected != "any" && expected != actual {
            anyhow::bail!(
                "The option `{name}` of plugin `{}` must be a `{expected}`, but the configuration sets it to a `{actual}`",
                context.spec.name
            );
        }
        Ok(value)
    }

    /// Declares a rule, which callbacks report diagnostics through with
    /// `rule.report(node, message)`. `fix_safety` is `"safe"`, `"unsafe"`, or
    /// `"display-only"`, and applies to all of the rule's fixes.
    fn rule<'v>(
        code: &str,
        name: &str,
        #[starlark(require = named, default = "safe")] fix_safety: &str,
        #[starlark(require = named)] url: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<RuleValue> {
        let context = LoadContext::require(eval, "`rule`")?;
        if !is_rule_code(code) {
            anyhow::bail!(
                "`{code}` is not a valid rule code; codes are uppercase letters followed by digits, like `ACME001`"
            );
        }
        if !is_rule_name(name) {
            anyhow::bail!("`{name}` is not a valid rule name; use kebab-case, like `no-print`");
        }
        let fix_safety = match fix_safety {
            "safe" => Applicability::Safe,
            "unsafe" => Applicability::Unsafe,
            "display-only" => Applicability::DisplayOnly,
            other => anyhow::bail!(
                "`fix_safety` must be `\"safe\"`, `\"unsafe\"`, or `\"display-only\"`, not `{other:?}`"
            ),
        };
        let mut rules = context.rules.borrow_mut();
        if let Some(existing) = rules
            .iter()
            .find(|rule| rule.code == code || rule.name == name)
        {
            anyhow::bail!(
                "The plugin already defines a rule `{}` ({})",
                existing.code,
                existing.name
            );
        }
        rules.push(RuleDef {
            code: code.to_string(),
            name: name.to_string(),
            fix_safety,
            url: url.map(str::to_string),
        });
        Ok(RuleValue {
            index: rules.len() - 1,
            code: code.to_string(),
            name: name.to_string(),
        })
    }
}
