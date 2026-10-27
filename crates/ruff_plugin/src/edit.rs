//! The edits of fixes: `replace(...)`, `delete(...)`, `insert_before(...)`, and
//! `insert_after(...)`.
//!
//! Edits know about Python's syntax, so that plugins don't need to:
//!
//! - Deleting a statement works like ruff's own fixes: it deletes the statement's line, or its
//!   `;`, or replaces it with `pass` if it's the only statement in its block.
//! - Deleting an item of a comma-separated list (an argument, an element, an imported name)
//!   deletes its comma. Deleting the only name of an import, or the value of an expression
//!   statement, deletes the statement.
//! - Inserting before or after a statement inserts lines with the statement's indentation, or
//!   uses `;` when the statement shares its line with another one.
//!
//! Deleting a node that can't be removed without breaking the code (the test of an `if`, say) is
//! an error, rather than a fix that ruff would discard.

use std::fmt;

use allocative::Allocative;
use ruff_diagnostics::Edit;
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt};
use ruff_python_trivia::{
    SimpleTokenKind, SimpleTokenizer, has_leading_content, has_trailing_content,
};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use starlark::any::ProvidesStaticType;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, StarlarkValue, starlark_value};

use crate::Host;
use crate::node::NodeRef;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditOp {
    Replace,
    Delete,
    InsertBefore,
    InsertAfter,
}

impl fmt::Display for EditOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EditOp::Replace => "replace",
            EditOp::Delete => "delete",
            EditOp::InsertBefore => "insert_before",
            EditOp::InsertAfter => "insert_after",
        })
    }
}

/// What an edit applies to.
#[derive(Clone, Debug)]
pub(crate) enum Target<'a> {
    /// A node, and the nodes that contain it (innermost first), with their ids.
    Node {
        node: NodeRef<'a>,
        ancestors: Vec<(u32, NodeRef<'a>)>,
    },
    /// A range of source text, such as an identifier.
    Range(TextRange),
    /// A module, which diagnostics (but not edits) can apply to.
    Module,
}

impl Target<'_> {
    pub(crate) fn range(&self) -> TextRange {
        match self {
            Target::Node { node, .. } => node.range(),
            Target::Range(range) => *range,
            Target::Module => TextRange::default(),
        }
    }
}

/// The Starlark value of an edit.
#[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct EditValue {
    #[allocative(skip)]
    pub(crate) edit: Edit,
    /// If the edit deletes a statement inside another statement, the id of that statement, and
    /// the range of the deleted one. A fix that deletes every statement of a block replaces one of
    /// them with `pass` instead (see [`crate::run::FileRun::report`]).
    #[allocative(skip)]
    pub(crate) deleted_statement: Option<(u32, TextRange)>,
}

impl From<Edit> for EditValue {
    fn from(edit: Edit) -> Self {
        Self {
            edit,
            deleted_statement: None,
        }
    }
}

pub(crate) fn edit<'a>(
    op: EditOp,
    target: &Target<'a>,
    text: &str,
    host: &dyn Host<'a>,
) -> anyhow::Result<EditValue> {
    if text.is_empty() && matches!(op, EditOp::InsertBefore | EditOp::InsertAfter) {
        anyhow::bail!("Inserting an empty string doesn't change anything");
    }
    match target {
        Target::Module => anyhow::bail!(
            "`{op}` edits nodes or spans, not modules; edit the module's first or last statement instead"
        ),
        Target::Range(range) => Ok(edit_range(op, *range, text).into()),
        Target::Node { node, ancestors } => match (op, node) {
            (_, NodeRef::Elif(_)) => anyhow::bail!(
                "`{op}` can't edit an `elif` clause; edit the statements in its body, or the whole `if` statement"
            ),
            (EditOp::Delete, node) => Deletion { host, ancestors }.node(*node),
            (EditOp::InsertBefore | EditOp::InsertAfter, NodeRef::Stmt(stmt)) => {
                Ok(insert_statement(op, stmt, text, host)?.into())
            }
            (_, node) => Ok(edit_range(op, node.range(), text).into()),
        },
    }
}

/// An edit of a range of text, which doesn't need to know what the text is.
fn edit_range(op: EditOp, range: TextRange, text: &str) -> Edit {
    match op {
        EditOp::Replace if text.is_empty() => Edit::range_deletion(range),
        EditOp::Replace => Edit::range_replacement(text.to_string(), range),
        EditOp::Delete => Edit::range_deletion(range),
        EditOp::InsertBefore => Edit::insertion(text.to_string(), range.start()),
        EditOp::InsertAfter => Edit::insertion(text.to_string(), range.end()),
    }
}

/// Inserts a line of code before or after a statement.
fn insert_statement<'a>(
    op: EditOp,
    stmt: &'a Stmt,
    text: &str,
    host: &dyn Host<'a>,
) -> anyhow::Result<Edit> {
    let source = host.source();
    let line_ending = host.line_ending();
    let starts_line = !has_leading_content(stmt.start(), source);
    let indent = &source[TextRange::new(source.line_start(stmt.start()), stmt.start())];
    // A statement that shares its line with another one (after a `;`, or after the `:` of a
    // compound statement) can only have code inserted next to it with a `;`.
    let on_same_line = |text: &str, edit: fn(&str) -> String, offset: TextSize| {
        if text.contains('\n') {
            anyhow::bail!(
                "`{op}` can't insert several lines next to a statement that shares its line with another one"
            );
        }
        Ok(Edit::insertion(edit(text), offset))
    };
    match op {
        EditOp::InsertBefore if starts_line => Ok(Edit::insertion(
            format!(
                "{indent}{}{line_ending}",
                indented(text, indent, line_ending)
            ),
            source.line_start(stmt.start()),
        )),
        EditOp::InsertBefore => on_same_line(text, |text| format!("{text}; "), stmt.start()),
        _ => {
            let line_end = source.line_end(stmt.end());
            let rest = source[TextRange::new(stmt.end(), line_end)].trim_start();
            let ends_line = rest.is_empty() || rest.starts_with('#');
            if starts_line && ends_line {
                // Insert after the line, and after any comment at the end of it.
                let text = indented(text, indent, line_ending);
                let full_line_end = source.full_line_end(stmt.end());
                Ok(if full_line_end == line_end {
                    // The statement is on the last line, which has no newline.
                    Edit::insertion(format!("{line_ending}{indent}{text}"), line_end)
                } else {
                    Edit::insertion(format!("{indent}{text}{line_ending}"), full_line_end)
                })
            } else {
                on_same_line(text, |text| format!("; {text}"), stmt.end())
            }
        }
    }
}

/// Indents the lines of `text` after the first with `indent`, so that inserted code stays in
/// the statement's block.
fn indented(text: &str, indent: &str, line_ending: &str) -> String {
    let mut lines = text.lines();
    let mut result = lines.next().unwrap_or_default().to_string();
    for line in lines {
        result.push_str(line_ending);
        if !line.trim().is_empty() {
            result.push_str(indent);
        }
        result.push_str(line);
    }
    result
}

/// Deletes nodes, given the nodes that contain them.
struct Deletion<'h, 'a> {
    host: &'h dyn Host<'a>,
    ancestors: &'h [(u32, NodeRef<'a>)],
}

impl<'a> Deletion<'_, 'a> {
    fn node(&self, node: NodeRef<'a>) -> anyhow::Result<EditValue> {
        let source = self.host.source();
        let parent = self.ancestors.first().map(|(_, parent)| *parent);
        match (node, parent) {
            (NodeRef::Comment(range), _) => Ok(delete_comment(range, source).into()),
            (NodeRef::Stmt(stmt), _) => Ok(self.statement(stmt, self.ancestors)),
            (NodeRef::Expr(expr), Some(parent)) => self.expression(expr, parent),
            (NodeRef::Ast(node), Some(parent)) => self.other(node, parent),
            _ => Err(cannot_delete(node, parent)),
        }
    }

    /// Deletes a statement, with `ancestors` being the nodes that contain it.
    fn statement(&self, stmt: &'a Stmt, ancestors: &[(u32, NodeRef<'a>)]) -> EditValue {
        // The statement whose block contains this one: `except` clauses and `match` cases
        // belong to their `try` and `match` statements, which ruff's helper knows about.
        let parent = ancestors.iter().find_map(|(id, ancestor)| match ancestor {
            NodeRef::Stmt(stmt) => Some((*id, *stmt)),
            _ => None,
        });
        EditValue {
            edit: self
                .host
                .delete_statement(stmt, parent.map(|(_, parent)| parent)),
            deleted_statement: parent.map(|(id, _)| (id, stmt.range())),
        }
    }

    fn expression(&self, expr: &'a Expr, parent: NodeRef<'a>) -> anyhow::Result<EditValue> {
        let source = self.host.source();
        let is = |other: &Expr| std::ptr::eq(other, expr);
        Ok(match parent {
            // Deleting the call of `breakpoint()` deletes the statement.
            NodeRef::Stmt(stmt @ Stmt::Expr(_)) => self.statement(stmt, &self.ancestors[1..]),
            NodeRef::Expr(Expr::Call(call)) if call.arguments.args.iter().any(is) => self
                .host
                .delete_argument(expr.range(), &call.arguments)?
                .into(),
            NodeRef::Stmt(Stmt::ClassDef(class)) if class.bases().iter().any(is) => {
                let arguments = class
                    .arguments
                    .as_deref()
                    .ok_or_else(|| cannot_delete(NodeRef::Expr(expr), Some(parent)))?;
                self.host.delete_argument(expr.range(), arguments)?.into()
            }
            NodeRef::Stmt(
                Stmt::FunctionDef(ast::StmtFunctionDef { decorator_list, .. })
                | Stmt::ClassDef(ast::StmtClassDef { decorator_list, .. }),
            ) if let Some(decorator) = decorator_list
                .iter()
                .find(|decorator| is(&decorator.expression)) =>
            {
                delete_line_or_range(decorator.range(), source).into()
            }
            NodeRef::Expr(Expr::Tuple(tuple)) => match tuple.elts.as_slice() {
                // `(a,)` becomes `()`, and `a,` too.
                [_] => Edit::range_replacement("()".to_string(), tuple.range()).into(),
                [first, second] => delete_pair_element(first, second, expr, source)?.into(),
                elts => delete_item(&ranges(elts), expr.range(), source)?.into(),
            },
            // `{a}` becomes `set()`, rather than the empty dict `{}`.
            NodeRef::Expr(Expr::Set(set)) if set.elts.len() == 1 => {
                Edit::range_replacement("set()".to_string(), set.range()).into()
            }
            NodeRef::Expr(
                Expr::Set(ast::ExprSet { elts, .. }) | Expr::List(ast::ExprList { elts, .. }),
            ) => delete_item(&ranges(elts), expr.range(), source)?.into(),
            NodeRef::Stmt(stmt @ Stmt::Delete(delete)) if delete.targets.iter().any(is) => {
                if delete.targets.len() == 1 {
                    self.statement(stmt, &self.ancestors[1..])
                } else {
                    delete_item(&ranges(&delete.targets), expr.range(), source)?.into()
                }
            }
            parent => return Err(cannot_delete(NodeRef::Expr(expr), Some(parent))),
        })
    }

    fn other(&self, node: AnyNodeRef<'a>, parent: NodeRef<'a>) -> anyhow::Result<EditValue> {
        let source = self.host.source();
        let error = || cannot_delete(NodeRef::Ast(node), Some(parent));
        Ok(match (node, parent) {
            (AnyNodeRef::Keyword(keyword), NodeRef::Expr(Expr::Call(call))) => self
                .host
                .delete_argument(keyword.range(), &call.arguments)?
                .into(),
            (AnyNodeRef::Keyword(keyword), NodeRef::Stmt(Stmt::ClassDef(class))) => {
                let arguments = class.arguments.as_deref().ok_or_else(error)?;
                self.host
                    .delete_argument(keyword.range(), arguments)?
                    .into()
            }
            (
                AnyNodeRef::Alias(alias),
                NodeRef::Stmt(
                    stmt @ (Stmt::Import(ast::StmtImport { names, .. })
                    | Stmt::ImportFrom(ast::StmtImportFrom { names, .. })),
                ),
            ) => {
                if names.len() == 1 {
                    self.statement(stmt, &self.ancestors[1..])
                } else {
                    delete_item(&ranges(names), alias.range(), source)?.into()
                }
            }
            (AnyNodeRef::WithItem(item), NodeRef::Stmt(Stmt::With(with)))
                if with.items.len() > 1 =>
            {
                delete_item(&ranges(&with.items), item.range(), source)?.into()
            }
            (
                AnyNodeRef::ParameterWithDefault(parameter),
                NodeRef::Ast(AnyNodeRef::Parameters(parameters)),
            ) => {
                // `def f(a, /)` and `def f(*, a)` need at least one parameter before the `/`,
                // and after the `*`.
                let is_only = |section: &[ast::ParameterWithDefault]| matches!(section, [only] if std::ptr::eq(only, parameter));
                if is_only(&parameters.posonlyargs)
                    || parameters.vararg.is_none() && is_only(&parameters.kwonlyargs)
                {
                    return Err(error());
                }
                let ranges: Vec<TextRange> = parameters
                    .iter()
                    .map(|parameter| parameter.range())
                    .collect();
                delete_item(&ranges, parameter.range(), source)?.into()
            }
            (
                AnyNodeRef::ExceptHandlerExceptHandler(handler),
                NodeRef::Stmt(Stmt::Try(try_stmt)),
            ) if try_stmt.handlers.len() > 1 || !try_stmt.finalbody.is_empty() => {
                delete_line_or_range(handler.range(), source).into()
            }
            (AnyNodeRef::MatchCase(case), NodeRef::Stmt(Stmt::Match(match_stmt)))
                if match_stmt.cases.len() > 1 =>
            {
                delete_line_or_range(case.range(), source).into()
            }
            _ => return Err(error()),
        })
    }
}

fn ranges<T: Ranged>(items: &[T]) -> Vec<TextRange> {
    items.iter().map(Ranged::range).collect()
}

fn cannot_delete(node: NodeRef, parent: Option<NodeRef>) -> anyhow::Error {
    let kind = node.kind().map_or("node", |kind| kind.name());
    match parent.and_then(NodeRef::kind) {
        Some(parent) => anyhow::anyhow!(
            "`delete` can't remove this `{kind}` from its `{parent}` without breaking the code; replace the `{parent}` (or part of it) instead"
        ),
        None => anyhow::anyhow!("`delete` can't remove this `{kind}`"),
    }
}

/// Deletes `expr` from a tuple of two elements, `first` and `second`. The remaining element keeps
/// a comma, so that the tuple stays a tuple: `(a, b)` becomes `(b,)`, not `(b)`.
fn delete_pair_element(
    first: &Expr,
    second: &Expr,
    expr: &Expr,
    source: &str,
) -> anyhow::Result<Edit> {
    let deletion = delete_item(&[first.range(), second.range()], expr.range(), source)?;
    // With a trailing comma, `(a, b,)`, deleting either element keeps that comma.
    if has_trailing_comma(second.range(), source) {
        return Ok(deletion);
    }
    let remaining = if std::ptr::eq(first, expr) {
        second
    } else {
        first
    }
    .range();
    let range = deletion.range().cover(remaining);
    let content = if remaining.start() < deletion.start() {
        format!(
            "{},{}",
            &source[remaining],
            &source[TextRange::new(remaining.end(), deletion.start())]
        )
    } else {
        format!(
            "{},",
            &source[TextRange::new(deletion.end(), remaining.end())]
        )
    };
    Ok(Edit::range_replacement(content, range))
}

/// Whether a comma follows `range`.
fn has_trailing_comma(range: TextRange, source: &str) -> bool {
    SimpleTokenizer::starts_at(range.end(), source)
        .skip_trivia()
        .next()
        .is_some_and(|token| token.kind() == SimpleTokenKind::Comma)
}

/// Deletes `item` from a comma-separated list of `items`, with the comma that separates it from
/// the next item (or, for the last item, from the previous one).
fn delete_item(items: &[TextRange], item: TextRange, source: &str) -> anyhow::Result<Edit> {
    let index = items
        .iter()
        .position(|range| *range == item)
        .ok_or_else(|| anyhow::anyhow!("The item isn't in its list"))?;
    if index + 1 < items.len() {
        // Delete the item, its comma, and the whitespace after it: `a, b` becomes `b`. A
        // comment after the comma stays.
        let mut tokens = SimpleTokenizer::starts_at(item.end(), source)
            .skip_while(|token| token.kind() != SimpleTokenKind::Comma)
            .skip(1);
        let end = tokens
            .find(|token| {
                !matches!(
                    token.kind(),
                    SimpleTokenKind::Whitespace | SimpleTokenKind::Newline
                )
            })
            .map_or(items[index + 1].start(), |token| token.start());
        Ok(Edit::deletion(item.start(), end))
    } else if let Some(previous) = index.checked_sub(1).map(|index| items[index]) {
        // Delete from the last comma before the item, so that a `/` or `*` between two
        // parameters stays: `a, /, b` becomes `a, /`.
        let comma = SimpleTokenizer::new(source, TextRange::new(previous.end(), item.start()))
            .filter(|token| token.kind() == SimpleTokenKind::Comma)
            .last()
            .ok_or_else(|| anyhow::anyhow!("Unable to find the comma before the item"))?;
        Ok(Edit::deletion(comma.start(), item.end()))
    } else {
        // The only item: `(a,)` becomes `()`.
        let end = SimpleTokenizer::starts_at(item.end(), source)
            .skip_trivia()
            .next()
            .filter(|token| token.kind() == SimpleTokenKind::Comma)
            .map_or(item.end(), |comma| comma.end());
        Ok(Edit::deletion(item.start(), end))
    }
}

/// Deletes the lines of `range` if nothing else is on them, or else just the range.
fn delete_line_or_range(range: TextRange, source: &str) -> Edit {
    if has_leading_content(range.start(), source) || has_trailing_content(range.end(), source) {
        Edit::range_deletion(range)
    } else {
        Edit::range_deletion(source.full_lines_range(range))
    }
}

/// Deletes a comment, and its line if nothing else is on it, like ruff's own fixes.
fn delete_comment(range: TextRange, source: &str) -> Edit {
    let line_start = source.line_start(range.start());
    let before = &source[TextRange::new(line_start, range.start())];
    if before.trim().is_empty() {
        Edit::range_deletion(source.full_lines_range(range))
    } else {
        // `x = 1  # comment` becomes `x = 1`.
        let spaces = before.len() - before.trim_end().len();
        let start = range.start() - TextSize::try_from(spaces).unwrap_or_default();
        Edit::deletion(start, range.end())
    }
}

starlark_simple_value!(EditValue);

impl fmt::Display for EditValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let range = self.edit.range();
        match self.edit.content() {
            Some(content) => write!(
                f,
                "<edit {}..{} -> {content:?}>",
                range.start().to_u32(),
                range.end().to_u32()
            ),
            None => write!(
                f,
                "<edit {}..{} -> deleted>",
                range.start().to_u32(),
                range.end().to_u32()
            ),
        }
    }
}

// `starlark_value` requires a named lifetime.
#[expect(clippy::elidable_lifetime_names)]
#[starlark_value(type = "Edit")]
impl<'v> StarlarkValue<'v> for EditValue {}
