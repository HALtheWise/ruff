//! Runtime values, and access to node fields in terms of Python's `ast` schema.

use std::borrow::Cow;
use std::ptr::NonNull;

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal};
use ruff_python_ast::{self as ast, AnyNodeRef, ElifElseClause, Expr, NodeKind, Number, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::kind::{Kind, bool_op_name, cmp_op_name, context_name, operator_name, unary_op_name};

/// A node that plugins can see.
#[derive(Copy, Clone, Debug)]
pub(crate) enum NodeRef<'a> {
    /// An expression. Expressions are kept as `&Expr`, rather than as an [`AnyNodeRef`], because
    /// ruff's semantic model needs an `&Expr` to resolve qualified names.
    Expr(&'a Expr),
    /// A statement, kept as a `&Stmt` for ruff's helpers that edit statements.
    Stmt(&'a Stmt),
    /// Any other node, such as a keyword argument.
    Ast(AnyNodeRef<'a>),
    /// Python's `ast` represents an `elif` as an `If` in the `orelse` of the preceding `If`,
    /// whereas ruff stores a flat list of `elif` and `else` clauses. This is the `If` that Python
    /// would create for the first of these clauses, which must be an `elif`.
    Elif(&'a [ElifElseClause]),
    Comment(TextRange),
}

/// Identifies a node. Nodes are identified by their address in ruff's AST, rather than by their
/// range, because different nodes can have the same range: the nodes parsed from a string
/// annotation like `"List[" "int]"` all have the range of the string.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NodeKey {
    /// A node of ruff's AST. Its kind distinguishes it from a child at the same address, such as
    /// the first field of a struct.
    Ast(NonNull<()>, NodeKind),
    Comment(TextRange),
}

impl<'a> NodeRef<'a> {
    pub(crate) fn key(self) -> NodeKey {
        let node = match self.any() {
            Some(node) => node,
            // The `If` of an `elif` is identified by its first clause.
            None => match self {
                NodeRef::Elif([clause, ..]) => AnyNodeRef::from(clause),
                _ => return NodeKey::Comment(self.range()),
            },
        };
        NodeKey::Ast(node.as_ptr(), node.kind())
    }

    /// The node of ruff's AST, unless this is an `elif` or a comment.
    pub(crate) fn any(self) -> Option<AnyNodeRef<'a>> {
        match self {
            NodeRef::Expr(expr) => Some(AnyNodeRef::from(expr)),
            NodeRef::Stmt(stmt) => Some(AnyNodeRef::from(stmt)),
            NodeRef::Ast(node) => Some(node),
            NodeRef::Elif(_) | NodeRef::Comment(_) => None,
        }
    }

    pub(crate) fn kind(self) -> Option<Kind> {
        match self {
            NodeRef::Expr(expr) => Kind::of(expr.into()),
            NodeRef::Stmt(stmt) => Kind::of(stmt.into()),
            NodeRef::Ast(node) => Kind::of(node),
            NodeRef::Elif(_) => Some(Kind::If),
            NodeRef::Comment(_) => Some(Kind::Comment),
        }
    }

    pub(crate) fn range(self) -> TextRange {
        match self {
            NodeRef::Expr(expr) => expr.range(),
            NodeRef::Stmt(stmt) => stmt.range(),
            NodeRef::Ast(node) => node.range(),
            NodeRef::Elif(clauses) => match (clauses.first(), clauses.last()) {
                (Some(first), Some(last)) => TextRange::new(first.start(), last.end()),
                _ => TextRange::default(),
            },
            NodeRef::Comment(range) => range,
        }
    }

    /// Returns `true` if both refer to the same node.
    pub(crate) fn is(self, other: NodeRef) -> bool {
        self.key() == other.key()
    }

    /// The nodes between this node and `descendant`, outermost first, or `None` if `descendant`
    /// isn't inside this node. For example, the nodes between a call and the value of one of its
    /// keyword arguments are the `keyword`.
    pub(crate) fn path_to(self, descendant: NodeRef<'a>) -> Option<Vec<NodeRef<'a>>> {
        // Most nodes are direct children of the statement or expression around them, which
        // these cases find without visiting the other children (of a long list, say).
        match (self, descendant) {
            // The value of a keyword argument is inside its `keyword`.
            (NodeRef::Expr(Expr::Call(call)), NodeRef::Expr(expr)) => {
                return Some(
                    call.arguments
                        .keywords
                        .iter()
                        .filter(|keyword| std::ptr::eq(std::ptr::from_ref(&keyword.value), expr))
                        .map(|keyword| NodeRef::Ast(AnyNodeRef::from(keyword)))
                        .collect(),
                );
            }
            (NodeRef::Expr(outer), NodeRef::Expr(_))
                if !matches!(
                    outer,
                    Expr::Lambda(_)
                        | Expr::ListComp(_)
                        | Expr::SetComp(_)
                        | Expr::DictComp(_)
                        | Expr::Generator(_)
                ) =>
            {
                return Some(Vec::new());
            }
            (NodeRef::Stmt(outer), NodeRef::Stmt(inner)) if is_in_own_block(outer, inner) => {
                return Some(Vec::new());
            }
            _ => {}
        }
        let target = descendant.key();
        let range = descendant.range();
        let mut path = Vec::new();
        self.find_path(target, range, &mut path).then_some(path)
    }

    fn find_path(self, target: NodeKey, range: TextRange, path: &mut Vec<NodeRef<'a>>) -> bool {
        let mut found = false;
        self.for_each_child(&mut |child| {
            if found || !child.range().contains_range(range) {
                return;
            }
            if child.key() == target {
                found = true;
                return;
            }
            // Siblings can contain the same range (in a string annotation, say), so search each
            // of them until one leads to the target.
            path.push(child);
            if child.find_path(target, range, path) {
                found = true;
            } else {
                path.pop();
            }
        });
        found
    }

    /// Calls `visit` for each direct child that plugins can see, in source order.
    pub(crate) fn for_each_child(self, visit: &mut dyn FnMut(NodeRef<'a>)) {
        let mut visitor = ChildVisitor { visit };
        match self {
            NodeRef::Expr(expr) => AnyNodeRef::from(expr).visit_source_order(&mut visitor),
            // A parameter with a default is a single `arg`: its children are those of the
            // parameter it wraps (the annotation), and the default.
            NodeRef::Ast(AnyNodeRef::ParameterWithDefault(parameter)) => {
                AnyNodeRef::from(&parameter.parameter).visit_source_order(&mut visitor);
                if let Some(default) = &parameter.default {
                    visitor.visit_expr(default);
                }
            }
            // Python represents an `elif` as an `If` in the `orelse` of the preceding `If`.
            NodeRef::Stmt(Stmt::If(if_stmt)) => {
                visitor.visit_expr(&if_stmt.test);
                visitor.visit_body(&if_stmt.body);
                for value in or_else(&if_stmt.elif_else_clauses) {
                    if let Val::Node(node) = value {
                        (visitor.visit)(node);
                    }
                }
            }
            NodeRef::Stmt(stmt) => AnyNodeRef::from(stmt).visit_source_order(&mut visitor),
            NodeRef::Ast(node) => node.visit_source_order(&mut visitor),
            NodeRef::Elif(clauses) => {
                if let Some(clause) = clauses.first() {
                    if let Some(test) = &clause.test {
                        visitor.visit_expr(test);
                    }
                    visitor.visit_body(&clause.body);
                    for value in or_else(&clauses[1..]) {
                        if let Val::Node(node) = value {
                            (visitor.visit)(node);
                        }
                    }
                }
            }
            NodeRef::Comment(_) => {}
        }
    }
}

/// Converts references to ruff AST nodes into [`NodeRef`]s.
pub(crate) trait AsNodeRef<'a> {
    fn node_ref(self) -> NodeRef<'a>;
}

impl<'a> AsNodeRef<'a> for &'a Expr {
    fn node_ref(self) -> NodeRef<'a> {
        NodeRef::Expr(self)
    }
}

impl<'a> AsNodeRef<'a> for &'a Stmt {
    fn node_ref(self) -> NodeRef<'a> {
        NodeRef::Stmt(self)
    }
}

macro_rules! as_node_ref {
    ($($ty:ty),*) => {
        $(
            impl<'a> AsNodeRef<'a> for &'a $ty {
                fn node_ref(self) -> NodeRef<'a> {
                    NodeRef::Ast(AnyNodeRef::from(self))
                }
            }
        )*
    };
}

as_node_ref!(
    ast::ExceptHandler,
    ast::Parameters,
    ast::Parameter,
    ast::ParameterWithDefault,
    ast::Keyword,
    ast::Alias,
    ast::WithItem,
    ast::Comprehension,
    ast::MatchCase
);

/// Visits the direct children of a node, looking through nodes that plugins don't see (such as
/// decorators and call arguments) to their children.
struct ChildVisitor<'v, 'a> {
    visit: &'v mut dyn FnMut(NodeRef<'a>),
}

impl<'a> SourceOrderVisitor<'a> for ChildVisitor<'_, 'a> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if Kind::of(node).is_some() {
            (self.visit)(NodeRef::Ast(node));
            TraversalSignal::Skip
        } else {
            TraversalSignal::Traverse
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        (self.visit)(NodeRef::Expr(expr));
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        (self.visit)(NodeRef::Stmt(stmt));
    }
}

/// A value produced by evaluating an expression against a row.
#[derive(Clone, Debug)]
pub(crate) enum Val<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Cow<'a, str>),
    /// An identifier, such as the `name` of a `FunctionDef`. Identifiers behave like strings, but
    /// remember where they are so that fixes can replace them.
    Ident(&'a str, TextRange),
    Node(NodeRef<'a>),
    List(Vec<Val<'a>>),
}

impl<'a> Val<'a> {
    pub(crate) fn node(node: impl AsNodeRef<'a>) -> Self {
        Val::Node(node.node_ref())
    }

    fn opt(node: Option<impl AsNodeRef<'a>>) -> Self {
        node.map_or(Val::Null, Val::node)
    }

    fn list<N: AsNodeRef<'a>>(nodes: impl IntoIterator<Item = N>) -> Self {
        Val::List(nodes.into_iter().map(Val::node).collect())
    }

    fn ident(identifier: &'a ast::Identifier) -> Self {
        Val::Ident(identifier.id.as_str(), identifier.range)
    }

    fn opt_ident(identifier: Option<&'a ast::Identifier>) -> Self {
        identifier.map_or(Val::Null, Val::ident)
    }

    fn str(value: &'static str) -> Self {
        Val::Str(Cow::Borrowed(value))
    }

    /// The source range of the value, if it has one.
    pub(crate) fn range(&self) -> Option<TextRange> {
        match self {
            Val::Ident(_, range) => Some(*range),
            Val::Node(node) => Some(node.range()),
            _ => None,
        }
    }
}

/// Returns the value of the field `name` of `node`, or [`Val::Null`] if the node has no such
/// field.
pub(crate) fn field<'a>(node: NodeRef<'a>, name: &str) -> Val<'a> {
    match node {
        NodeRef::Expr(expr) => expr_field(expr, name),
        NodeRef::Stmt(stmt) => ast_field(stmt.into(), name),
        NodeRef::Ast(node) => ast_field(node, name),
        NodeRef::Elif(clauses) => {
            let Some(clause) = clauses.first() else {
                return Val::Null;
            };
            match name {
                "test" => clause.test.as_ref().map_or(Val::Null, Val::node),
                "body" => Val::list(&clause.body),
                "orelse" => Val::List(or_else(&clauses[1..])),
                _ => Val::Null,
            }
        }
        // The text of a comment is its source text, which the `text` attribute of every node
        // already holds.
        NodeRef::Comment(_) => Val::Null,
    }
}

/// Python's `orelse` for an `if` statement whose remaining `elif` and `else` clauses are
/// `clauses`.
fn or_else(clauses: &[ElifElseClause]) -> Vec<Val<'_>> {
    match clauses.first() {
        None => Vec::new(),
        Some(clause) if clause.test.is_some() => vec![Val::Node(NodeRef::Elif(clauses))],
        Some(clause) => clause.body.iter().map(Val::node).collect(),
    }
}

fn stmts(body: &[Stmt]) -> Val<'_> {
    Val::list(body)
}

fn exprs(elts: &[Expr]) -> Val<'_> {
    Val::list(elts)
}

fn ast_field<'a>(node: AnyNodeRef<'a>, name: &str) -> Val<'a> {
    match (node, name) {
        // Statements
        (AnyNodeRef::StmtFunctionDef(function), _) => match name {
            "name" => Val::ident(&function.name),
            "args" => Val::node(&*function.parameters),
            "body" => stmts(&function.body),
            "decorator_list" => Val::List(
                function
                    .decorator_list
                    .iter()
                    .map(|d| Val::node(&d.expression))
                    .collect(),
            ),
            "returns" => Val::opt(function.returns.as_deref()),
            _ => Val::Null,
        },
        (AnyNodeRef::StmtClassDef(class), _) => match name {
            "name" => Val::ident(&class.name),
            "bases" => exprs(class.bases()),
            "keywords" => Val::list(class.keywords()),
            "body" => stmts(&class.body),
            "decorator_list" => Val::List(
                class
                    .decorator_list
                    .iter()
                    .map(|d| Val::node(&d.expression))
                    .collect(),
            ),
            _ => Val::Null,
        },
        (AnyNodeRef::StmtReturn(ret), "value") => Val::opt(ret.value.as_deref()),
        (AnyNodeRef::StmtDelete(delete), "targets") => exprs(&delete.targets),
        (AnyNodeRef::StmtTypeAlias(alias), "name") => Val::node(&*alias.name),
        (AnyNodeRef::StmtTypeAlias(alias), "value") => Val::node(&*alias.value),
        (AnyNodeRef::StmtAssign(assign), "targets") => exprs(&assign.targets),
        (AnyNodeRef::StmtAssign(assign), "value") => Val::node(&*assign.value),
        (AnyNodeRef::StmtAugAssign(assign), _) => match name {
            "target" => Val::node(&*assign.target),
            "op" => Val::str(operator_name(assign.op)),
            "value" => Val::node(&*assign.value),
            _ => Val::Null,
        },
        (AnyNodeRef::StmtAnnAssign(assign), _) => match name {
            "target" => Val::node(&*assign.target),
            "annotation" => Val::node(&*assign.annotation),
            "value" => Val::opt(assign.value.as_deref()),
            "simple" => Val::Int(i64::from(assign.simple)),
            _ => Val::Null,
        },
        (AnyNodeRef::StmtFor(for_stmt), _) => match name {
            "target" => Val::node(&*for_stmt.target),
            "iter" => Val::node(&*for_stmt.iter),
            "body" => stmts(&for_stmt.body),
            "orelse" => stmts(&for_stmt.orelse),
            _ => Val::Null,
        },
        (AnyNodeRef::StmtWhile(while_stmt), _) => match name {
            "test" => Val::node(&*while_stmt.test),
            "body" => stmts(&while_stmt.body),
            "orelse" => stmts(&while_stmt.orelse),
            _ => Val::Null,
        },
        (AnyNodeRef::StmtIf(if_stmt), _) => match name {
            "test" => Val::node(&*if_stmt.test),
            "body" => stmts(&if_stmt.body),
            "orelse" => Val::List(or_else(&if_stmt.elif_else_clauses)),
            _ => Val::Null,
        },
        (AnyNodeRef::StmtWith(with), "items") => Val::list(&with.items),
        (AnyNodeRef::StmtWith(with), "body") => stmts(&with.body),
        (AnyNodeRef::StmtMatch(match_stmt), "subject") => Val::node(&*match_stmt.subject),
        (AnyNodeRef::StmtMatch(match_stmt), "cases") => Val::list(&match_stmt.cases),
        (AnyNodeRef::StmtRaise(raise), "exc") => Val::opt(raise.exc.as_deref()),
        (AnyNodeRef::StmtRaise(raise), "cause") => Val::opt(raise.cause.as_deref()),
        (AnyNodeRef::StmtTry(try_stmt), _) => match name {
            "body" => stmts(&try_stmt.body),
            "handlers" => Val::List(try_stmt.handlers.iter().map(Val::node).collect()),
            "orelse" => stmts(&try_stmt.orelse),
            "finalbody" => stmts(&try_stmt.finalbody),
            _ => Val::Null,
        },
        (AnyNodeRef::StmtAssert(assert), "test") => Val::node(&*assert.test),
        (AnyNodeRef::StmtAssert(assert), "msg") => Val::opt(assert.msg.as_deref()),
        (AnyNodeRef::StmtImport(import), "names") => Val::list(&import.names),
        (AnyNodeRef::StmtImportFrom(import), _) => match name {
            "module" => Val::opt_ident(import.module.as_ref()),
            "names" => Val::list(&import.names),
            "level" => Val::Int(i64::from(import.level)),
            _ => Val::Null,
        },
        (AnyNodeRef::StmtGlobal(global), "names") => {
            Val::List(global.names.iter().map(Val::ident).collect())
        }
        (AnyNodeRef::StmtNonlocal(nonlocal), "names") => {
            Val::List(nonlocal.names.iter().map(Val::ident).collect())
        }
        (AnyNodeRef::StmtExpr(expr), "value") => Val::node(&*expr.value),

        // Everything else
        (AnyNodeRef::ExceptHandlerExceptHandler(handler), _) => match name {
            "type" => Val::opt(handler.type_.as_deref()),
            "name" => Val::opt_ident(handler.name.as_ref()),
            "body" => stmts(&handler.body),
            _ => Val::Null,
        },
        (AnyNodeRef::Parameters(parameters), _) => parameters_field(parameters, name),
        (AnyNodeRef::Parameter(parameter), _) => match name {
            "arg" => Val::ident(&parameter.name),
            "annotation" => Val::opt(parameter.annotation.as_deref()),
            _ => Val::Null,
        },
        (AnyNodeRef::ParameterWithDefault(parameter), _) => match name {
            "default" => Val::opt(parameter.default.as_deref()),
            _ => ast_field(AnyNodeRef::Parameter(&parameter.parameter), name),
        },
        (AnyNodeRef::Keyword(keyword), "arg") => Val::opt_ident(keyword.arg.as_ref()),
        (AnyNodeRef::Keyword(keyword), "value") => Val::node(&keyword.value),
        (AnyNodeRef::Alias(alias), "name") => Val::ident(&alias.name),
        (AnyNodeRef::Alias(alias), "asname") => Val::opt_ident(alias.asname.as_ref()),
        (AnyNodeRef::WithItem(item), "context_expr") => Val::node(&item.context_expr),
        (AnyNodeRef::WithItem(item), "optional_vars") => Val::opt(item.optional_vars.as_deref()),
        (AnyNodeRef::Comprehension(comp), _) => match name {
            "target" => Val::node(&comp.target),
            "iter" => Val::node(&comp.iter),
            "ifs" => exprs(&comp.ifs),
            "is_async" => Val::Int(i64::from(comp.is_async)),
            _ => Val::Null,
        },
        (AnyNodeRef::MatchCase(case), "guard") => Val::opt(case.guard.as_deref()),
        (AnyNodeRef::MatchCase(case), "body") => stmts(&case.body),
        _ => Val::Null,
    }
}

fn expr_field<'a>(expr: &'a Expr, name: &str) -> Val<'a> {
    match (expr, name) {
        (Expr::BoolOp(bool_op), "op") => Val::str(bool_op_name(bool_op.op)),
        (Expr::BoolOp(bool_op), "values") => exprs(&bool_op.values),
        (Expr::Named(named), "target") => Val::node(&*named.target),
        (Expr::Named(named), "value") => Val::node(&*named.value),
        (Expr::BinOp(bin_op), _) => match name {
            "left" => Val::node(&*bin_op.left),
            "op" => Val::str(operator_name(bin_op.op)),
            "right" => Val::node(&*bin_op.right),
            _ => Val::Null,
        },
        (Expr::UnaryOp(unary_op), "op") => Val::str(unary_op_name(unary_op.op)),
        (Expr::UnaryOp(unary_op), "operand") => Val::node(&*unary_op.operand),
        (Expr::Lambda(lambda), "args") => Val::opt(lambda.parameters.as_deref()),
        (Expr::Lambda(lambda), "body") => Val::node(&*lambda.body),
        (Expr::If(if_exp), _) => match name {
            "test" => Val::node(&*if_exp.test),
            "body" => Val::node(&*if_exp.body),
            "orelse" => Val::node(&*if_exp.orelse),
            _ => Val::Null,
        },
        (Expr::Dict(dict), "keys") => Val::List(dict.iter_keys().map(Val::opt).collect()),
        (Expr::Dict(dict), "values") => Val::List(dict.iter_values().map(Val::node).collect()),
        (Expr::Set(set), "elts") => exprs(&set.elts),
        (Expr::ListComp(comp), "elt") => Val::node(&*comp.elt),
        (Expr::ListComp(comp), "generators") => Val::list(&comp.generators),
        (Expr::SetComp(comp), "elt") => Val::node(&*comp.elt),
        (Expr::SetComp(comp), "generators") => Val::list(&comp.generators),
        (Expr::Generator(comp), "elt") => Val::node(&*comp.elt),
        (Expr::Generator(comp), "generators") => Val::list(&comp.generators),
        (Expr::DictComp(comp), _) => match name {
            "key" => Val::opt(comp.key.as_deref()),
            "value" => Val::node(&*comp.value),
            "generators" => Val::list(&*comp.generators),
            _ => Val::Null,
        },
        (Expr::Await(await_expr), "value") => Val::node(&*await_expr.value),
        (Expr::Yield(yield_expr), "value") => Val::opt(yield_expr.value.as_deref()),
        (Expr::YieldFrom(yield_from), "value") => Val::node(&*yield_from.value),
        (Expr::Compare(compare), _) => match name {
            "left" => Val::node(compare.first_operand()),
            "ops" => Val::List(
                compare
                    .ops
                    .iter()
                    .map(|op| Val::str(cmp_op_name(*op)))
                    .collect(),
            ),
            "comparators" => exprs(compare.comparators()),
            _ => Val::Null,
        },
        (Expr::Call(call), _) => match name {
            "func" => Val::node(&*call.func),
            "args" => exprs(&call.arguments.args),
            "keywords" => Val::list(&call.arguments.keywords),
            _ => Val::Null,
        },
        (
            Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_),
            "value",
        ) => literal(expr),
        (Expr::Attribute(attribute), _) => match name {
            "value" => Val::node(&*attribute.value),
            "attr" => Val::ident(&attribute.attr),
            "ctx" => Val::str(context_name(attribute.ctx)),
            _ => Val::Null,
        },
        (Expr::Subscript(subscript), _) => match name {
            "value" => Val::node(&*subscript.value),
            "slice" => Val::node(&*subscript.slice),
            "ctx" => Val::str(context_name(subscript.ctx)),
            _ => Val::Null,
        },
        (Expr::Starred(starred), "value") => Val::node(&*starred.value),
        (Expr::Starred(starred), "ctx") => Val::str(context_name(starred.ctx)),
        (Expr::Name(name_expr), "id") => Val::Ident(name_expr.id.as_str(), name_expr.range),
        (Expr::Name(name_expr), "ctx") => Val::str(context_name(name_expr.ctx)),
        (Expr::List(list), "elts") => exprs(&list.elts),
        (Expr::List(list), "ctx") => Val::str(context_name(list.ctx)),
        (Expr::Tuple(tuple), "elts") => exprs(&tuple.elts),
        (Expr::Tuple(tuple), "ctx") => Val::str(context_name(tuple.ctx)),
        (Expr::Slice(slice), _) => match name {
            "lower" => Val::opt(slice.lower.as_deref()),
            "upper" => Val::opt(slice.upper.as_deref()),
            "step" => Val::opt(slice.step.as_deref()),
            _ => Val::Null,
        },

        _ => Val::Null,
    }
}

fn parameters_field<'a>(parameters: &'a ast::Parameters, name: &str) -> Val<'a> {
    match name {
        "posonlyargs" => Val::list(&parameters.posonlyargs),
        "args" => Val::list(&parameters.args),
        "vararg" => Val::opt(parameters.vararg.as_deref()),
        "kwonlyargs" => Val::list(&parameters.kwonlyargs),
        "kwarg" => Val::opt(parameters.kwarg.as_deref()),
        "kw_defaults" => Val::List(
            parameters
                .kwonlyargs
                .iter()
                .map(|parameter| Val::opt(parameter.default.as_deref()))
                .collect(),
        ),
        // Python lists the defaults of the trailing positional parameters.
        "defaults" => Val::List(
            parameters
                .posonlyargs
                .iter()
                .chain(&parameters.args)
                .filter_map(|parameter| parameter.default.as_deref().map(Val::node))
                .collect(),
        ),
        _ => Val::Null,
    }
}

/// The value of a literal, following `ast.literal_eval` for constants, negated numbers, and
/// list, tuple, and set displays. Returns [`Val::Null`] for anything else.
pub(crate) fn literal(expr: &Expr) -> Val<'_> {
    match expr {
        Expr::StringLiteral(string) => Val::Str(Cow::Borrowed(string.value.to_str())),
        Expr::BytesLiteral(bytes) => Val::Str(Cow::Owned(
            String::from_utf8_lossy(&bytes.value.bytes().collect::<Vec<u8>>()).into_owned(),
        )),
        Expr::NumberLiteral(number) => match &number.value {
            Number::Int(int) => int.as_i64().map_or(Val::Null, Val::Int),
            Number::Float(float) => Val::Float(*float),
            Number::Complex { .. } => Val::Null,
        },
        Expr::BooleanLiteral(boolean) => Val::Bool(boolean.value),
        Expr::UnaryOp(ast::ExprUnaryOp {
            op: ast::UnaryOp::USub,
            operand,
            ..
        }) => match literal(operand) {
            Val::Int(value) => Val::Int(-value),
            Val::Float(value) => Val::Float(-value),
            _ => Val::Null,
        },
        Expr::List(ast::ExprList { elts, .. })
        | Expr::Tuple(ast::ExprTuple { elts, .. })
        | Expr::Set(ast::ExprSet { elts, .. }) => {
            let mut values = Vec::with_capacity(elts.len());
            for elt in elts {
                match literal(elt) {
                    Val::Null if !elt.is_none_literal_expr() => return Val::Null,
                    value => values.push(value),
                }
            }
            Val::List(values)
        }
        _ => Val::Null,
    }
}

/// Whether `inner` is a statement of one of the blocks of `outer` that belong to `outer` itself,
/// rather than to one of its clauses (`except`, `case`, `elif`, `else` after an `elif`).
fn is_in_own_block(outer: &Stmt, inner: &Stmt) -> bool {
    let blocks: &[&[Stmt]] = match outer {
        Stmt::FunctionDef(ast::StmtFunctionDef { body, .. })
        | Stmt::ClassDef(ast::StmtClassDef { body, .. })
        | Stmt::With(ast::StmtWith { body, .. }) => &[body],
        Stmt::For(ast::StmtFor { body, orelse, .. })
        | Stmt::While(ast::StmtWhile { body, orelse, .. }) => &[body, orelse],
        Stmt::If(ast::StmtIf { body, .. }) => &[body],
        Stmt::Try(ast::StmtTry {
            body,
            orelse,
            finalbody,
            ..
        }) => &[body, orelse, finalbody],
        _ => &[],
    };
    // The statements of a block are in source order, and don't overlap.
    blocks.iter().any(|block| {
        block
            .binary_search_by_key(&inner.start(), Ranged::start)
            .is_ok_and(|index| std::ptr::eq(std::ptr::from_ref(&block[index]), inner))
    })
}

/// The blocks of statements of a compound statement, as ruff stores them: an `if` statement's
/// blocks include those of its `elif` and `else` clauses, and a `try` statement's those of its
/// `except` clauses.
pub(crate) fn blocks(stmt: &Stmt) -> Vec<&[Stmt]> {
    match stmt {
        Stmt::FunctionDef(ast::StmtFunctionDef { body, .. })
        | Stmt::ClassDef(ast::StmtClassDef { body, .. })
        | Stmt::With(ast::StmtWith { body, .. }) => vec![body],
        Stmt::For(ast::StmtFor { body, orelse, .. })
        | Stmt::While(ast::StmtWhile { body, orelse, .. }) => vec![body, orelse],
        Stmt::If(if_stmt) => std::iter::once(&*if_stmt.body)
            .chain(if_stmt.elif_else_clauses.iter().map(|clause| &*clause.body))
            .collect(),
        Stmt::Try(try_stmt) => [&*try_stmt.body, &try_stmt.orelse, &try_stmt.finalbody]
            .into_iter()
            .chain(try_stmt.handlers.iter().map(|handler| match handler {
                ast::ExceptHandler::ExceptHandler(handler) => &*handler.body,
            }))
            .collect(),
        Stmt::Match(match_stmt) => match_stmt.cases.iter().map(|case| &*case.body).collect(),
        _ => Vec::new(),
    }
}
