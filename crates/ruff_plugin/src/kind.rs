//! The node kinds that plugins can query, named after the classes in Python's `ast` module.
//!
//! Plugin authors already know Python's `ast` module (and so do the language models that write
//! many lint rules), so plugins see ruff's AST through that schema: `Call` has `func`, `args`, and
//! `keywords`, string and number literals are `Constant`s, `async def` is an `AsyncFunctionDef`,
//! and so on. [`crate::node`] implements the field access for each kind.

use std::fmt;

use ruff_python_ast::{self as ast, AnyNodeRef};

macro_rules! kinds {
    ($($variant:ident => $name:tt [$($field:literal),*],)*) => {
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
        pub(crate) enum Kind {
            $($variant,)*
        }

        impl Kind {
            pub(crate) const ALL: &[Kind] = &[$(Kind::$variant,)*];

            pub(crate) const fn name(self) -> &'static str {
                match self {
                    $(Kind::$variant => $name,)*
                }
            }

            /// The fields of this kind, in the same order as the `_fields` of the corresponding
            /// class in Python's `ast` module.
            pub(crate) const fn fields(self) -> &'static [&'static str] {
                match self {
                    $(Kind::$variant => &[$($field),*],)*
                }
            }
        }
    };
}

/// Calls `$callback!` with the table of kinds: each kind's variant, its name in Python's `ast`
/// module, and its fields.
macro_rules! all_kinds {
    ($callback:ident) => {
        $callback! {
    // Statements
    FunctionDef => "FunctionDef" ["name", "args", "body", "decorator_list", "returns"],
    AsyncFunctionDef => "AsyncFunctionDef" ["name", "args", "body", "decorator_list", "returns"],
    ClassDef => "ClassDef" ["name", "bases", "keywords", "body", "decorator_list"],
    Return => "Return" ["value"],
    Delete => "Delete" ["targets"],
    Assign => "Assign" ["targets", "value"],
    TypeAlias => "TypeAlias" ["name", "value"],
    AugAssign => "AugAssign" ["target", "op", "value"],
    AnnAssign => "AnnAssign" ["target", "annotation", "value", "simple"],
    For => "For" ["target", "iter", "body", "orelse"],
    AsyncFor => "AsyncFor" ["target", "iter", "body", "orelse"],
    While => "While" ["test", "body", "orelse"],
    If => "If" ["test", "body", "orelse"],
    With => "With" ["items", "body"],
    AsyncWith => "AsyncWith" ["items", "body"],
    Match => "Match" ["subject", "cases"],
    Raise => "Raise" ["exc", "cause"],
    Try => "Try" ["body", "handlers", "orelse", "finalbody"],
    TryStar => "TryStar" ["body", "handlers", "orelse", "finalbody"],
    Assert => "Assert" ["test", "msg"],
    Import => "Import" ["names"],
    ImportFrom => "ImportFrom" ["module", "names", "level"],
    Global => "Global" ["names"],
    Nonlocal => "Nonlocal" ["names"],
    ExprStmt => "Expr" ["value"],
    Pass => "Pass" [],
    Break => "Break" [],
    Continue => "Continue" [],
    IpyEscapeCommand => "IpyEscapeCommand" [],

    // Expressions
    BoolOp => "BoolOp" ["op", "values"],
    NamedExpr => "NamedExpr" ["target", "value"],
    BinOp => "BinOp" ["left", "op", "right"],
    UnaryOp => "UnaryOp" ["op", "operand"],
    Lambda => "Lambda" ["args", "body"],
    IfExp => "IfExp" ["test", "body", "orelse"],
    Dict => "Dict" ["keys", "values"],
    Set => "Set" ["elts"],
    ListComp => "ListComp" ["elt", "generators"],
    SetComp => "SetComp" ["elt", "generators"],
    DictComp => "DictComp" ["key", "value", "generators"],
    GeneratorExp => "GeneratorExp" ["elt", "generators"],
    Await => "Await" ["value"],
    Yield => "Yield" ["value"],
    YieldFrom => "YieldFrom" ["value"],
    Compare => "Compare" ["left", "ops", "comparators"],
    Call => "Call" ["func", "args", "keywords"],
    JoinedStr => "JoinedStr" [],
    TemplateStr => "TemplateStr" [],
    Constant => "Constant" ["value"],
    Attribute => "Attribute" ["value", "attr", "ctx"],
    Subscript => "Subscript" ["value", "slice", "ctx"],
    Starred => "Starred" ["value", "ctx"],
    Name => "Name" ["id", "ctx"],
    List => "List" ["elts", "ctx"],
    Tuple => "Tuple" ["elts", "ctx"],
    Slice => "Slice" ["lower", "upper", "step"],

    // Everything else
    ExceptHandler => "ExceptHandler" ["type", "name", "body"],
    Arguments => "arguments" ["posonlyargs", "args", "vararg", "kwonlyargs", "kw_defaults", "kwarg", "defaults"],
    // `default` is not a field of Python's `ast.arg`, but it is much more convenient than lining
    // up `arguments.defaults` with `arguments.args`.
    Arg => "arg" ["arg", "annotation", "default"],
    Keyword => "keyword" ["arg", "value"],
    Alias => "alias" ["name", "asname"],
    WithItem => "withitem" ["context_expr", "optional_vars"],
    Comprehension => "comprehension" ["target", "iter", "ifs", "is_async"],
    MatchCase => "match_case" ["guard", "body"],
    Comment => "Comment" ["text"],
        }
    };
}

pub(crate) use all_kinds;

all_kinds!(kinds);

// `KindSet` is a bitset of kinds.
const _: () = assert!(Kind::ALL.len() <= 128);

impl Kind {
    /// Whether a field of this kind holds plain data (strings, numbers, and lists of them), as
    /// opposed to nodes. Plugins read data fields as attributes, and call methods for the fields
    /// that hold nodes.
    pub(crate) fn is_data_field(self, field: &str) -> bool {
        match (self, field) {
            // `type X = ...` names its alias with a `Name` node.
            (Kind::TypeAlias, "name") => false,
            (Kind::Constant, "value") | (Kind::Global | Kind::Nonlocal, "names") => true,
            (Kind::Comment, "text") => true,
            (
                _,
                "name" | "id" | "attr" | "arg" | "asname" | "module" | "level" | "op" | "ctx"
                | "ops" | "simple" | "is_async",
            ) => true,
            _ => false,
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.iter().copied().find(|kind| kind.name() == name)
    }

    pub(crate) const fn is_statement(self) -> bool {
        (self as u8) <= (Kind::IpyEscapeCommand as u8)
    }

    pub(crate) const fn is_expression(self) -> bool {
        (self as u8) >= (Kind::BoolOp as u8) && (self as u8) <= (Kind::Slice as u8)
    }

    /// Returns the kind of a ruff AST node, or `None` for nodes that plugins never see directly
    /// (for example, decorators, which Python's `ast` represents as plain expressions).
    pub(crate) fn of(node: AnyNodeRef) -> Option<Kind> {
        Some(match node {
            AnyNodeRef::StmtFunctionDef(function) if function.is_async => Kind::AsyncFunctionDef,
            AnyNodeRef::StmtFunctionDef(_) => Kind::FunctionDef,
            AnyNodeRef::StmtClassDef(_) => Kind::ClassDef,
            AnyNodeRef::StmtReturn(_) => Kind::Return,
            AnyNodeRef::StmtDelete(_) => Kind::Delete,
            AnyNodeRef::StmtTypeAlias(_) => Kind::TypeAlias,
            AnyNodeRef::StmtAssign(_) => Kind::Assign,
            AnyNodeRef::StmtAugAssign(_) => Kind::AugAssign,
            AnyNodeRef::StmtAnnAssign(_) => Kind::AnnAssign,
            AnyNodeRef::StmtFor(for_stmt) if for_stmt.is_async => Kind::AsyncFor,
            AnyNodeRef::StmtFor(_) => Kind::For,
            AnyNodeRef::StmtWhile(_) => Kind::While,
            AnyNodeRef::StmtIf(_) => Kind::If,
            AnyNodeRef::StmtWith(with) if with.is_async => Kind::AsyncWith,
            AnyNodeRef::StmtWith(_) => Kind::With,
            AnyNodeRef::StmtMatch(_) => Kind::Match,
            AnyNodeRef::StmtRaise(_) => Kind::Raise,
            AnyNodeRef::StmtTry(try_stmt) if try_stmt.is_star => Kind::TryStar,
            AnyNodeRef::StmtTry(_) => Kind::Try,
            AnyNodeRef::StmtAssert(_) => Kind::Assert,
            AnyNodeRef::StmtImport(_) => Kind::Import,
            AnyNodeRef::StmtImportFrom(_) => Kind::ImportFrom,
            AnyNodeRef::StmtGlobal(_) => Kind::Global,
            AnyNodeRef::StmtNonlocal(_) => Kind::Nonlocal,
            AnyNodeRef::StmtExpr(_) => Kind::ExprStmt,
            AnyNodeRef::StmtPass(_) => Kind::Pass,
            AnyNodeRef::StmtBreak(_) => Kind::Break,
            AnyNodeRef::StmtContinue(_) => Kind::Continue,
            AnyNodeRef::StmtIpyEscapeCommand(_) | AnyNodeRef::ExprIpyEscapeCommand(_) => {
                Kind::IpyEscapeCommand
            }
            AnyNodeRef::ExprBoolOp(_) => Kind::BoolOp,
            AnyNodeRef::ExprNamed(_) => Kind::NamedExpr,
            AnyNodeRef::ExprBinOp(_) => Kind::BinOp,
            AnyNodeRef::ExprUnaryOp(_) => Kind::UnaryOp,
            AnyNodeRef::ExprLambda(_) => Kind::Lambda,
            AnyNodeRef::ExprIf(_) => Kind::IfExp,
            AnyNodeRef::ExprDict(_) => Kind::Dict,
            AnyNodeRef::ExprSet(_) => Kind::Set,
            AnyNodeRef::ExprListComp(_) => Kind::ListComp,
            AnyNodeRef::ExprSetComp(_) => Kind::SetComp,
            AnyNodeRef::ExprDictComp(_) => Kind::DictComp,
            AnyNodeRef::ExprGenerator(_) => Kind::GeneratorExp,
            AnyNodeRef::ExprAwait(_) => Kind::Await,
            AnyNodeRef::ExprYield(_) => Kind::Yield,
            AnyNodeRef::ExprYieldFrom(_) => Kind::YieldFrom,
            AnyNodeRef::ExprCompare(_) => Kind::Compare,
            AnyNodeRef::ExprCall(_) => Kind::Call,
            AnyNodeRef::ExprFString(_) => Kind::JoinedStr,
            AnyNodeRef::ExprTString(_) => Kind::TemplateStr,
            AnyNodeRef::ExprStringLiteral(_)
            | AnyNodeRef::ExprBytesLiteral(_)
            | AnyNodeRef::ExprNumberLiteral(_)
            | AnyNodeRef::ExprBooleanLiteral(_)
            | AnyNodeRef::ExprNoneLiteral(_)
            | AnyNodeRef::ExprEllipsisLiteral(_) => Kind::Constant,
            AnyNodeRef::ExprAttribute(_) => Kind::Attribute,
            AnyNodeRef::ExprSubscript(_) => Kind::Subscript,
            AnyNodeRef::ExprStarred(_) => Kind::Starred,
            AnyNodeRef::ExprName(_) => Kind::Name,
            AnyNodeRef::ExprList(_) => Kind::List,
            AnyNodeRef::ExprTuple(_) => Kind::Tuple,
            AnyNodeRef::ExprSlice(_) => Kind::Slice,
            AnyNodeRef::ExceptHandlerExceptHandler(_) => Kind::ExceptHandler,
            AnyNodeRef::Parameters(_) => Kind::Arguments,
            AnyNodeRef::Parameter(_) | AnyNodeRef::ParameterWithDefault(_) => Kind::Arg,
            AnyNodeRef::Keyword(_) => Kind::Keyword,
            AnyNodeRef::Alias(_) => Kind::Alias,
            AnyNodeRef::WithItem(_) => Kind::WithItem,
            AnyNodeRef::Comprehension(_) => Kind::Comprehension,
            AnyNodeRef::MatchCase(_) => Kind::MatchCase,
            _ => return None,
        })
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A set of [`Kind`]s, as accepted by `nodes(...)`, `is_kind(...)`, `descendants(...)`, and so on.
///
/// Besides the names of concrete kinds, authors can use Python's abstract base classes `stmt`
/// and `expr`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct KindSet(u128);

impl KindSet {
    pub(crate) fn of(kinds: &[Kind]) -> Self {
        let mut set = Self::default();
        for kind in kinds {
            set.insert(*kind);
        }
        set
    }

    pub(crate) const fn union(self, other: KindSet) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) fn contains(self, kind: Kind) -> bool {
        self.0 & (1 << kind as u8) != 0
    }

    pub(crate) fn insert(&mut self, kind: Kind) {
        self.0 |= 1 << kind as u8;
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = Kind> {
        Kind::ALL
            .iter()
            .copied()
            .filter(move |kind| self.contains(*kind))
    }

    /// Parses kind names, reporting unknown names with a suggestion.
    pub(crate) fn parse<'s>(names: impl IntoIterator<Item = &'s str>) -> anyhow::Result<Self> {
        let mut set = KindSet::default();
        for name in names {
            match name {
                "stmt" => Kind::ALL
                    .iter()
                    .filter(|kind| kind.is_statement())
                    .for_each(|kind| set.insert(*kind)),
                "expr" => Kind::ALL
                    .iter()
                    .filter(|kind| kind.is_expression())
                    .for_each(|kind| set.insert(*kind)),
                "Comment" => anyhow::bail!(
                    "Comments aren't nodes of the syntax tree; select them with `comments(...)`"
                ),
                _ if name
                    .strip_prefix("ast.")
                    .is_some_and(|kind| Kind::from_name(kind).is_some()) =>
                {
                    anyhow::bail!(
                        "Node kinds are plain names: use `{:?}`, not `{name:?}`",
                        &name["ast.".len()..]
                    );
                }
                _ => {
                    let Some(kind) = Kind::from_name(name) else {
                        let candidates = Kind::ALL
                            .iter()
                            .map(|kind| kind.name())
                            .chain(["stmt", "expr"]);
                        anyhow::bail!(
                            "Unknown node kind `{name}`{}",
                            crate::did_you_mean(name, candidates)
                        );
                    };
                    set.insert(kind);
                }
            }
        }
        Ok(set)
    }
}

impl fmt::Display for KindSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let statements = Kind::ALL.iter().filter(|kind| kind.is_statement());
        let expressions = Kind::ALL.iter().filter(|kind| kind.is_expression());
        let mut names: Vec<&str> = Vec::new();
        let mut rest = *self;
        for (category, members) in [
            ("stmt", statements.copied().collect::<Vec<_>>()),
            ("expr", expressions.copied().collect::<Vec<_>>()),
        ] {
            if members.iter().all(|kind| rest.contains(*kind)) {
                names.push(category);
                for kind in &members {
                    rest.0 &= !(1 << *kind as u8);
                }
            }
        }
        names.extend(rest.iter().map(Kind::name));
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "\"{name}\"")?;
        }
        Ok(())
    }
}

pub(crate) const fn operator_name(op: ast::Operator) -> &'static str {
    match op {
        ast::Operator::Add => "Add",
        ast::Operator::Sub => "Sub",
        ast::Operator::Mult => "Mult",
        ast::Operator::MatMult => "MatMult",
        ast::Operator::Div => "Div",
        ast::Operator::Mod => "Mod",
        ast::Operator::Pow => "Pow",
        ast::Operator::LShift => "LShift",
        ast::Operator::RShift => "RShift",
        ast::Operator::BitOr => "BitOr",
        ast::Operator::BitXor => "BitXor",
        ast::Operator::BitAnd => "BitAnd",
        ast::Operator::FloorDiv => "FloorDiv",
    }
}

pub(crate) const fn cmp_op_name(op: ast::CmpOp) -> &'static str {
    match op {
        ast::CmpOp::Eq => "Eq",
        ast::CmpOp::NotEq => "NotEq",
        ast::CmpOp::Lt => "Lt",
        ast::CmpOp::LtE => "LtE",
        ast::CmpOp::Gt => "Gt",
        ast::CmpOp::GtE => "GtE",
        ast::CmpOp::Is => "Is",
        ast::CmpOp::IsNot => "IsNot",
        ast::CmpOp::In => "In",
        ast::CmpOp::NotIn => "NotIn",
    }
}

pub(crate) const fn unary_op_name(op: ast::UnaryOp) -> &'static str {
    match op {
        ast::UnaryOp::Invert => "Invert",
        ast::UnaryOp::Not => "Not",
        ast::UnaryOp::UAdd => "UAdd",
        ast::UnaryOp::USub => "USub",
    }
}

pub(crate) const fn bool_op_name(op: ast::BoolOp) -> &'static str {
    match op {
        ast::BoolOp::And => "And",
        ast::BoolOp::Or => "Or",
    }
}

pub(crate) const fn context_name(ctx: ast::ExprContext) -> &'static str {
    match ctx {
        ast::ExprContext::Load | ast::ExprContext::Invalid => "Load",
        ast::ExprContext::Store => "Store",
        ast::ExprContext::Del => "Del",
    }
}
