//! The values that plugin callbacks receive: nodes, modules, and spans of source text, plus
//! rules, which callbacks report diagnostics through.
//!
//! Values hold what's cheap to know about them as attributes: a node's `text`, `line`,
//! `qualname`, and the fields of Python's `ast` that hold plain data, like `function.name`.
//! Fields that hold other nodes are methods, like `call.func()`: methods receive the evaluator,
//! through which they reach the file being linted (see [`crate::run::with_run`]), whereas
//! attributes don't. This is also what keeps nodes cheap: a node is a handle, and only reads the
//! parts of ruff's AST that a callback asks for.
//!
//! Each kind of node is its own Starlark type, named after its class in Python's `ast` (so
//! `type(node)` is `"Call"`), because Starlark looks up methods by type: a `Constant`'s `value`
//! is data, but an `Assign`'s `value` is a node.

use std::fmt;
use std::hash::Hash;

use allocative::Allocative;
use ruff_python_ast::{AnyNodeRef, Expr, Stmt};
use ruff_source_file::SourceFile;
use ruff_text_size::TextRange;
use starlark::any::ProvidesStaticType;
use starlark::collections::StarlarkHasher;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::list::{AllocList, ListRef};
use starlark::values::none::NoneType;
use starlark::values::tuple::UnpackTuple;
use starlark::values::{
    Demand, Heap, NoSerialize, StarlarkValue, Value, ValueLike, starlark_value,
};

use crate::edit::{EditOp, EditValue, Target};
use crate::kind::Kind;
use crate::node::{NodeRef, Val, field, literal};
use crate::run::{Active, RunContext, register_exit, run_context, with_run};
use crate::selector::{self, Scope, SelectorValue};

/// The name that `filter_name` matches: the name of a definition, the `id` of a `Name`, the
/// `attr` of an `Attribute`, the `arg` of a parameter or keyword argument, the `name` of an
/// imported name, or the `name` of an exception handler.
pub(crate) fn name_of(node: NodeRef<'_>) -> Option<&str> {
    Some(match node {
        NodeRef::Expr(Expr::Name(name)) => name.id.as_str(),
        NodeRef::Expr(Expr::Attribute(attribute)) => attribute.attr.as_str(),
        NodeRef::Stmt(Stmt::FunctionDef(function)) => function.name.as_str(),
        NodeRef::Stmt(Stmt::ClassDef(class)) => class.name.as_str(),
        NodeRef::Ast(AnyNodeRef::Parameter(parameter)) => parameter.name.as_str(),
        NodeRef::Ast(AnyNodeRef::ParameterWithDefault(parameter)) => {
            parameter.parameter.name.as_str()
        }
        NodeRef::Ast(AnyNodeRef::Keyword(keyword)) => keyword.arg.as_ref()?.as_str(),
        NodeRef::Ast(AnyNodeRef::Alias(alias)) => alias.name.as_str(),
        NodeRef::Ast(AnyNodeRef::ExceptHandlerExceptHandler(handler)) => {
            handler.name.as_ref()?.as_str()
        }
        _ => return None,
    })
}

/// The value of a field that holds plain data.
#[derive(Debug, Clone)]
pub(crate) enum Data {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Data>),
}

impl Data {
    pub(crate) fn new(value: &Val) -> Self {
        match value {
            Val::Bool(value) => Data::Bool(*value),
            Val::Int(value) => Data::Int(*value),
            Val::Float(value) => Data::Float(*value),
            Val::Str(value) => Data::Str(value.to_string()),
            Val::Ident(value, _) => Data::Str((*value).to_string()),
            Val::List(values) => Data::List(values.iter().map(Data::new).collect()),
            Val::Null | Val::Node(_) => Data::None,
        }
    }

    fn to_value<'v>(&self, heap: &'v Heap) -> Value<'v> {
        match self {
            Data::None => Value::new_none(),
            Data::Bool(value) => Value::new_bool(*value),
            Data::Int(value) => heap.alloc(*value),
            Data::Float(value) => heap.alloc(*value),
            Data::Str(value) => heap.alloc(value.as_str()),
            Data::List(values) => {
                heap.alloc(AllocList(values.iter().map(|value| value.to_value(heap))))
            }
        }
    }
}

/// A node of the file being linted. Its Starlark value is one of the types in [`node_types`],
/// which wrap it.
#[derive(Debug, Clone, ProvidesStaticType, Allocative)]
pub(crate) struct NodeValue {
    /// The generation of the file run that the node belongs to.
    pub(crate) file: u64,
    /// The node's index in the run's table of nodes.
    pub(crate) id: u32,
    #[allocative(skip)]
    pub(crate) kind: Kind,
    #[allocative(skip)]
    pub(crate) range: TextRange,
    /// The file, for the node's text and line.
    #[allocative(skip)]
    pub(crate) source: SourceFile,
    /// The qualified name that the node refers to, resolved when the value was created, which is
    /// while ruff visits the node (or the node that the callback that created it received).
    pub(crate) qualname: Option<String>,
    /// The fields that hold plain data (see [`Kind::is_data_field`]).
    #[allocative(skip)]
    pub(crate) data: Vec<(&'static str, Data)>,
}

impl NodeValue {
    fn entry<'a>(&self, active: &Active<'a, '_>) -> anyhow::Result<(NodeRef<'a>, Option<u32>)> {
        active.check_file(self.file)?;
        active
            .run
            .entry(self.id)
            .ok_or_else(|| anyhow::anyhow!("Unknown node"))
    }

    fn text(&self) -> &str {
        self.source
            .source_text()
            .get(self.range.start().to_usize()..self.range.end().to_usize())
            .unwrap_or_default()
    }

    /// Converts a field's value, interning the nodes it contains as children of this node.
    fn convert<'a, 'v>(
        &self,
        active: &Active<'a, '_>,
        value: &Val<'a>,
        heap: &'v Heap,
    ) -> Value<'v> {
        match value {
            Val::Null => Value::new_none(),
            Val::Bool(value) => Value::new_bool(*value),
            Val::Int(value) => heap.alloc(*value),
            Val::Float(value) => heap.alloc(*value),
            Val::Str(value) => heap.alloc(value.as_ref()),
            Val::Ident(value, _) => heap.alloc(*value),
            Val::Node(child) => {
                let id = active.run.intern_within(*child, self.id);
                alloc_node(active, id, heap)
            }
            Val::List(values) => heap.alloc(AllocList(
                values.iter().map(|value| self.convert(active, value, heap)),
            )),
        }
    }
}

/// Allocates the value of the node with id `id`.
fn alloc_node<'v>(active: &Active<'_, '_>, id: u32, heap: &'v Heap) -> Value<'v> {
    match active.run.node_value(id, active.host) {
        Some(node) => node.alloc(heap),
        None => Value::new_none(),
    }
}

impl fmt::Display for NodeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MAX: usize = 40;
        let text = self.text();
        let line = text.lines().next().unwrap_or_default();
        if line.len() < text.len() || line.chars().count() > MAX {
            let short: String = line.chars().take(MAX).collect();
            write!(f, "<{} {short}...>", self.kind)
        } else {
            write!(f, "<{} {line}>", self.kind)
        }
    }
}

const NODE_ATTRIBUTES: [&str; 3] = ["text", "line", "qualname"];

impl NodeValue {
    fn get_attr<'v>(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        Some(match attribute {
            "text" => heap.alloc(self.text()),
            "line" => heap.alloc(self.source.index().line_index(self.range.start()).get()),
            "qualname" => match &self.qualname {
                Some(qualname) => heap.alloc(qualname.as_str()),
                None => Value::new_none(),
            },
            _ => self
                .data
                .iter()
                .find(|(name, _)| *name == attribute)?
                .1
                .to_value(heap),
        })
    }

    fn has_attr(&self, attribute: &str) -> bool {
        NODE_ATTRIBUTES.contains(&attribute) || self.data.iter().any(|(name, _)| *name == attribute)
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut attributes: Vec<String> = NODE_ATTRIBUTES.map(str::to_string).to_vec();
        for (name, _) in &self.data {
            if !NODE_ATTRIBUTES.contains(name) {
                attributes.push((*name).to_string());
            }
        }
        attributes
    }

    fn equals(&self, other: Value) -> bool {
        as_node(other).is_some_and(|other| other.file == self.file && other.id == self.id)
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) {
        self.file.hash(hasher);
        self.id.hash(hasher);
    }
}

/// Defines a Starlark type for each kind of node, wrapping a [`NodeValue`].
macro_rules! node_types {
    ($($variant:ident => $name:tt [$($field:literal),*],)*) => {
        pub(crate) mod node_types {
            use super::*;

            $(
                #[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
                pub(crate) struct $variant(pub(crate) NodeValue);

                starlark_simple_value!($variant);

                impl fmt::Display for $variant {
                    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        fmt::Display::fmt(&self.0, f)
                    }
                }

                #[starlark_value(type = $name)]
                impl<'v> StarlarkValue<'v> for $variant {
                    fn get_methods() -> Option<&'static Methods> {
                        static METHODS: MethodsStatic = MethodsStatic::new();
                        METHODS.methods(|builder| node_methods(Kind::$variant, builder))
                    }

                    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
                        self.0.get_attr(attribute, heap)
                    }

                    fn has_attr(&self, attribute: &str, _heap: &'v Heap) -> bool {
                        self.0.has_attr(attribute)
                    }

                    fn dir_attr(&self) -> Vec<String> {
                        self.0.dir_attr()
                    }

                    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
                        Ok(self.0.equals(other))
                    }

                    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
                        self.0.write_hash(hasher);
                        Ok(())
                    }

                    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
                        demand.provide_value::<&NodeValue>(&self.0);
                    }
                }
            )*
        }

        impl NodeValue {
            /// Allocates the node's Starlark value, of the type for its kind.
            pub(crate) fn alloc(self, heap: &Heap) -> Value<'_> {
                match self.kind {
                    $(Kind::$variant => heap.alloc(node_types::$variant(self)),)*
                }
            }
        }
    };
}

crate::kind::all_kinds!(node_types);

/// The node of a Starlark value, if it's a node.
pub(crate) fn as_node(value: Value<'_>) -> Option<&NodeValue> {
    value.request_value::<&NodeValue>()
}

fn this_node(value: Value<'_>) -> anyhow::Result<&NodeValue> {
    as_node(value)
        .ok_or_else(|| anyhow::anyhow!("Expected a node, but got a `{}`", value.get_type()))
}

/// The file being linted, which `modules().foreach(...)` callbacks receive.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct ModuleValue {
    pub(crate) file: u64,
    /// The path of the file, relative to the plugin's root.
    pub(crate) path: String,
    /// The dotted name of the module, if it's in a package.
    pub(crate) name: Option<String>,
}

starlark_simple_value!(ModuleValue);

impl fmt::Display for ModuleValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<Module {}>", self.path)
    }
}

const MODULE_ATTRIBUTES: [&str; 2] = ["path", "name"];

#[starlark_value(type = "Module")]
impl<'v> StarlarkValue<'v> for ModuleValue {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new();
        METHODS.methods(scope_methods)
    }

    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        Some(match attribute {
            "path" => heap.alloc(self.path.as_str()),
            "name" => match &self.name {
                Some(name) => heap.alloc(name.as_str()),
                None => Value::new_none(),
            },
            _ => return None,
        })
    }

    fn has_attr(&self, attribute: &str, _heap: &'v Heap) -> bool {
        MODULE_ATTRIBUTES.contains(&attribute)
    }

    fn dir_attr(&self) -> Vec<String> {
        MODULE_ATTRIBUTES.map(str::to_string).to_vec()
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<ModuleValue>()
            .is_some_and(|other| other.file == self.file))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.file.hash(hasher);
        Ok(())
    }
}

/// A range of source text that isn't a node, such as the name of a function.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct SpanValue {
    file: u64,
    #[allocative(skip)]
    range: TextRange,
    #[allocative(skip)]
    source: SourceFile,
}

starlark_simple_value!(SpanValue);

impl SpanValue {
    fn text(&self) -> &str {
        self.source
            .source_text()
            .get(self.range.start().to_usize()..self.range.end().to_usize())
            .unwrap_or_default()
    }
}

impl fmt::Display for SpanValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<Span {}>", self.text())
    }
}

#[starlark_value(type = "Span")]
impl<'v> StarlarkValue<'v> for SpanValue {
    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        Some(match attribute {
            "text" => heap.alloc(self.text()),
            "line" => heap.alloc(self.source.index().line_index(self.range.start()).get()),
            _ => return None,
        })
    }

    fn has_attr(&self, attribute: &str, _heap: &'v Heap) -> bool {
        matches!(attribute, "text" | "line")
    }

    fn dir_attr(&self) -> Vec<String> {
        ["text", "line"].map(str::to_string).to_vec()
    }
}

/// The scope of the selectors made from a module or node, and the prefix that displays them.
fn scope_of(value: Value) -> anyhow::Result<(Scope, &'static str)> {
    if let Some(node) = as_node(value) {
        Ok((Scope::Node(node.file, node.id), "node."))
    } else if let Some(module) = value.downcast_ref::<ModuleValue>() {
        Ok((Scope::File(module.file), "module."))
    } else {
        anyhow::bail!(
            "Expected a node or module, but got a `{}`",
            value.get_type()
        )
    }
}

/// The methods of both modules and nodes, which find nodes inside them.
#[starlark_module]
fn scope_methods(builder: &mut MethodsBuilder) {
    /// The nodes inside this one of the given kinds (see the global `nodes`).
    fn nodes<'v>(
        this: Value<'v>,
        #[starlark(args)] kinds: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        let (scope, prefix) = scope_of(this)?;
        Ok(SelectorValue::new(selector::nodes(
            &kinds.items,
            scope,
            prefix,
        )?))
    }

    /// The calls inside this node (see the global `calls`).
    fn calls<'v>(
        this: Value<'v>,
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        let (scope, prefix) = scope_of(this)?;
        Ok(SelectorValue::new(selector::calls(
            &patterns.items,
            scope,
            prefix,
        )?))
    }

    /// The names and attributes inside this node (see the global `references`).
    fn references<'v>(
        this: Value<'v>,
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        let (scope, prefix) = scope_of(this)?;
        Ok(SelectorValue::new(selector::references(
            &patterns.items,
            scope,
            prefix,
        )?))
    }

    /// The imported names inside this node (see the global `imports`).
    fn imports<'v>(
        this: Value<'v>,
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        let (scope, prefix) = scope_of(this)?;
        Ok(SelectorValue::new(selector::imports(
            &patterns.items,
            scope,
            prefix,
        )?))
    }

    /// The comments inside this node (see the global `comments`).
    fn comments<'v>(
        this: Value<'v>,
        #[starlark(args)] patterns: UnpackTuple<&'v str>,
    ) -> anyhow::Result<SelectorValue> {
        let (scope, prefix) = scope_of(this)?;
        Ok(SelectorValue::new(selector::comments(
            &patterns.items,
            scope,
            prefix,
        )?))
    }

    /// Calls `callback` (with no arguments) once ruff has visited everything inside this node, and
    /// every callback for those nodes has run.
    fn on_exit<'v>(
        this: Value<'v>,
        callback: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        if let Some(node) = as_node(this) {
            register_exit(eval, node.file, Some(node.id), callback)?;
        } else if let Some(module) = this.downcast_ref::<ModuleValue>() {
            register_exit(eval, module.file, None, callback)?;
        }
        Ok(NoneType)
    }
}

/// The methods of nodes of `kind`: those of every node, and one for each of its fields that
/// holds nodes.
fn node_methods(kind: Kind, builder: &mut MethodsBuilder) {
    scope_methods(builder);
    other_node_methods(builder);
    if kind.is_expression() {
        expression_methods(builder);
    }
    if kind == Kind::Call {
        call_methods(builder);
    }
    for field in kind.fields() {
        if !kind.is_data_field(field) {
            field_methods::register(field, builder);
        }
    }
}

/// The value of the field `name` of a node.
fn field_method<'v>(
    this: Value<'v>,
    name: &str,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    let this = this_node(this)?;
    let heap = eval.heap();
    with_run(run_context(eval)?, |active| {
        let (node, _) = this.entry(active)?;
        Ok(this.convert(active, &field(node, name), heap))
    })
}

/// Defines a method for each field of Python's `ast` classes that holds nodes, each in a module
/// of its own, so that the types of each kind of node can register the methods of their fields.
macro_rules! field_methods {
    ($($field:ident),* $(,)?) => {
        pub(crate) mod field_methods {
            use super::*;

            $(
                mod $field {
                    use super::*;

                    #[starlark_module]
                    pub(super) fn register(builder: &mut MethodsBuilder) {
                        fn $field<'v>(
                            this: Value<'v>,
                            eval: &mut Evaluator<'v, '_, '_>,
                        ) -> anyhow::Result<Value<'v>> {
                            field_method(this, stringify!($field).trim_start_matches("r#"), eval)
                        }
                    }
                }
            )*

            /// The fields that have methods.
            #[cfg(test)]
            pub(crate) const NAMES: &[&str] = &[$(stringify!($field)),*];

            /// Registers the method of the field `name`.
            pub(super) fn register(name: &str, builder: &mut MethodsBuilder) {
                $(
                    if name == stringify!($field).trim_start_matches("r#") {
                        $field::register(builder);
                    }
                )*
            }
        }
    };
}

// The fields of every kind in `kind.rs` that hold nodes in some kind.
field_methods!(
    annotation,
    args,
    bases,
    body,
    cases,
    cause,
    comparators,
    context_expr,
    decorator_list,
    default,
    defaults,
    elt,
    elts,
    exc,
    finalbody,
    func,
    generators,
    guard,
    handlers,
    ifs,
    items,
    iter,
    key,
    keys,
    keywords,
    kw_defaults,
    kwarg,
    kwonlyargs,
    left,
    lower,
    msg,
    name,
    names,
    operand,
    optional_vars,
    orelse,
    posonlyargs,
    returns,
    right,
    slice,
    step,
    subject,
    target,
    targets,
    test,
    r#type,
    upper,
    value,
    values,
    vararg,
);

#[starlark_module]
fn other_node_methods(builder: &mut MethodsBuilder) {
    /// The node that contains this one, or `None` for a statement at the top level of a module.
    fn parent<'v>(this: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let this = this_node(this)?;
        let heap = eval.heap();
        with_run(run_context(eval)?, |active| {
            Ok(match this.entry(active)?.1 {
                Some(parent) => alloc_node(active, parent, heap),
                None => Value::new_none(),
            })
        })
    }

    /// The innermost node that contains this one and has one of the given kinds, or `None`.
    fn enclosing<'v>(
        this: Value<'v>,
        #[starlark(args)] kinds: UnpackTuple<&'v str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let this = this_node(this)?;
        let kinds = selector::kinds(&kinds.items)?;
        let heap = eval.heap();
        with_run(run_context(eval)?, |active| {
            let (_, mut parent) = this.entry(active)?;
            while let Some(id) = parent {
                let Some((node, next)) = active.run.entry(id) else {
                    break;
                };
                if node.kind().is_some_and(|kind| kinds.contains(kind)) {
                    return Ok(alloc_node(active, id, heap));
                }
                parent = next;
            }
            Ok(Value::new_none())
        })
    }

    /// The source range of the field `name`, such as the name of a function, to report a
    /// diagnostic on or edit.
    fn span<'v>(
        this: Value<'v>,
        name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<SpanValue> {
        let this = this_node(this)?;
        let range = with_run(run_context(eval)?, |active| {
            let (node, _) = this.entry(active)?;
            field(node, name).range().ok_or_else(|| {
                anyhow::anyhow!(
                    "`{}` nodes have no field `{name}` with a position",
                    this.kind
                )
            })
        })?;
        Ok(SpanValue {
            file: this.file,
            range,
            source: this.source.clone(),
        })
    }
}

/// The methods of expressions.
#[starlark_module]
fn expression_methods(builder: &mut MethodsBuilder) {
    /// The value of the expression, as `ast.literal_eval` computes it, or `None` if it isn't a
    /// literal.
    fn literal<'v>(this: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let this = this_node(this)?;
        let heap = eval.heap();
        with_run(run_context(eval)?, |active| {
            Ok(match this.entry(active)?.0 {
                NodeRef::Expr(expr) => this.convert(active, &literal(expr), heap),
                _ => Value::new_none(),
            })
        })
    }
}

/// The methods of calls.
#[starlark_module]
fn call_methods(builder: &mut MethodsBuilder) {
    /// An argument of a call: `argument(0)` is the first positional argument,
    /// `argument("timeout")` the value of the keyword argument `timeout`, and
    /// `argument("url", 0)` an argument that can be passed either way. Returns `None` if the
    /// call doesn't pass the argument.
    fn argument<'v>(
        this: Value<'v>,
        key: Value<'v>,
        position: Option<i32>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let this = this_node(this)?;
        let to_position = |position: i32| {
            usize::try_from(position)
                .map_err(|_| anyhow::anyhow!("Argument positions start at 0, not {position}"))
        };
        let (name, position) = if let Some(index) = key.unpack_i32() {
            if position.is_some() {
                anyhow::bail!(
                    "Use `argument(name, position)` to find an argument by name or position"
                );
            }
            (None, Some(to_position(index)?))
        } else if let Some(name) = key.unpack_str() {
            (Some(name), position.map(to_position).transpose()?)
        } else {
            anyhow::bail!(
                "`argument` expects a position or a name, but got a `{}`",
                key.get_type()
            );
        };
        let heap = eval.heap();
        with_run(run_context(eval)?, |active| {
            let (NodeRef::Expr(Expr::Call(call)), _) = this.entry(active)? else {
                anyhow::bail!("`argument` only applies to calls");
            };
            let found = name
                .and_then(|name| {
                    call.arguments
                        .find_keyword(name)
                        .map(|keyword| &keyword.value)
                })
                .or_else(|| position.and_then(|position| call.arguments.find_positional(position)));
            Ok(match found {
                // The value of a keyword argument is inside its `keyword`, which `intern_within`
                // finds.
                Some(value) => {
                    let id = active.run.intern_within(NodeRef::Expr(value), this.id);
                    alloc_node(active, id, heap)
                }
                None => Value::new_none(),
            })
        })
    }

    /// The `keyword` node of the keyword argument `name` of a call, or `None`.
    fn keyword<'v>(
        this: Value<'v>,
        name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let this = this_node(this)?;
        let heap = eval.heap();
        with_run(run_context(eval)?, |active| {
            let (NodeRef::Expr(Expr::Call(call)), _) = this.entry(active)? else {
                anyhow::bail!("`keyword` only applies to calls");
            };
            Ok(match call.arguments.find_keyword(name) {
                Some(keyword) => {
                    let id = active
                        .run
                        .intern_within(NodeRef::Ast(AnyNodeRef::from(keyword)), this.id);
                    alloc_node(active, id, heap)
                }
                None => Value::new_none(),
            })
        })
    }
}

/// A rule, as returned by `rule(...)`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct RuleValue {
    /// The rule's index among the rules of its plugin.
    pub(crate) index: usize,
    pub(crate) code: String,
    pub(crate) name: String,
}

starlark_simple_value!(RuleValue);

impl fmt::Display for RuleValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<rule {} {}>", self.code, self.name)
    }
}

#[starlark_value(type = "Rule")]
impl<'v> StarlarkValue<'v> for RuleValue {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new();
        METHODS.methods(rule_methods)
    }

    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        match attribute {
            "code" => Some(heap.alloc(self.code.as_str())),
            "name" => Some(heap.alloc(self.name.as_str())),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["code", "name"].map(str::to_string).to_vec()
    }
}

/// The edits of a `fix` argument: an edit, a list of edits, or `None`.
fn edits(fix: Value) -> anyhow::Result<Vec<EditValue>> {
    if fix.is_none() {
        Ok(Vec::new())
    } else if let Some(edit) = fix.downcast_ref::<EditValue>() {
        Ok(vec![edit.clone()])
    } else if let Some(list) = ListRef::from_value(fix) {
        list.content()
            .iter()
            .map(|edit| match edit.downcast_ref::<EditValue>() {
                Some(edit) => Ok(edit.clone()),
                None => anyhow::bail!(
                    "`fix` must be an edit or a list of edits, but the list contains a `{}`",
                    edit.get_type()
                ),
            })
            .collect()
    } else {
        anyhow::bail!(
            "`fix` must be an edit (like `replace(node, \"...\")`) or a list of edits, not a `{}`",
            fix.get_type()
        )
    }
}

/// The node, span, or module that a diagnostic or edit applies to.
fn target<'a>(active: &Active<'a, '_>, value: Value) -> anyhow::Result<Target<'a>> {
    if let Some(node) = as_node(value) {
        let (node, mut parent) = node.entry(active)?;
        let mut ancestors = Vec::new();
        while let Some(id) = parent
            && let Some((ancestor, next)) = active.run.entry(id)
        {
            ancestors.push((id, ancestor));
            parent = next;
        }
        Ok(Target::Node { node, ancestors })
    } else if let Some(span) = value.downcast_ref::<SpanValue>() {
        active.check_file(span.file)?;
        Ok(Target::Range(span.range))
    } else if let Some(module) = value.downcast_ref::<ModuleValue>() {
        active.check_file(module.file)?;
        Ok(Target::Module)
    } else {
        anyhow::bail!(
            "Expected a node (or a span, from `node.span(...)`), but got a `{}`",
            value.get_type()
        )
    }
}

/// Creates an edit of `target`.
pub(crate) fn make_edit(
    op: EditOp,
    target: Value,
    text: &str,
    context: &RunContext<'_>,
) -> anyhow::Result<EditValue> {
    with_run(context, |active| {
        let target = self::target(active, target)?;
        crate::edit::edit(op, &target, text, active.host)
    })
}

#[starlark_module]
fn rule_methods(builder: &mut MethodsBuilder) {
    /// Reports a diagnostic on `node` (or a span or module), with an optional fix: an edit, or a
    /// list of edits, from `replace`, `delete`, `insert_before`, or `insert_after`.
    fn report<'v>(
        this: &RuleValue,
        node: Value<'v>,
        message: &str,
        #[starlark(require = named, default = NoneType)] fix: Value<'v>,
        #[starlark(require = named)] fix_title: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let edits = edits(fix)?;
        // `fix_title` is ignored without a fix, so that a plugin can pass both whether or not
        // it found a fix: `fix = fix, fix_title = "..."`.
        let fix_title = fix_title.filter(|_| !edits.is_empty());
        with_run(run_context(eval)?, |active| {
            let range = target(active, node)?.range();
            active.run.report(
                active.plugin,
                this.index,
                range,
                message.to_string(),
                edits,
                fix_title.map(str::to_string),
            )
        })?;
        Ok(NoneType)
    }
}
