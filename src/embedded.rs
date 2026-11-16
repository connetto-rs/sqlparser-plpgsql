//! Reading the SQL a PL/pgSQL function body runs, as a tree of the body's
//! blocks, branches, loops and exception handlers, with every statement that
//! leaves one of them where it is written.
//!
//! PostgreSQL evaluates every PL/pgSQL expression as a `SELECT`, so each one
//! appears here as the `SELECT` it runs, and [`expression`] recovers the
//! expression itself. Names the body declares and the function's parameters
//! are left as written, and so is every variable a statement assigns. Each
//! block lists the names it declares, with the types written for them and
//! unresolved, which a caller holding the schema can resolve, and
//! [`crate::namespace`] answers which of them a name reaches.

use alloc::{
    borrow::Cow,
    boxed::Box,
    collections::BTreeSet,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ops::Range;

use sqlparser::{
    ast::{
        DataType, Expr, GroupByExpr, Ident, ObjectName, Query, Select, SelectItem, SetExpr,
        Statement, Value,
    },
    dialect::PostgreSqlDialect,
    keywords::Keyword,
    parser::{Parser, ParserError},
    tokenizer::{Span, Token, Tokenizer, Word},
};

use crate::{
    Region, Scanner,
    namespace::{Level, stored, stored_parts},
};

/// How deep blocks and control structures may nest before reading stops,
/// which keeps a hostile body from exhausting the stack.
const NESTING_LIMIT: usize = 50;

/// An error produced while reading the SQL a PL/pgSQL body embeds.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The body could not be tokenized.
    #[error("PL/pgSQL body could not be tokenized: {source}")]
    UntokenizableBody {
        /// The tokenizer error.
        #[source]
        source: sqlparser::tokenizer::TokenizerError,
    },

    /// The body does not have the shape PL/pgSQL requires.
    #[error("PL/pgSQL body expected {expected}, found {found}")]
    Syntax {
        /// What the grammar requires at this point.
        expected: &'static str,
        /// The token found instead.
        found: String,
    },

    /// An SQL statement or expression the body embeds could not be parsed.
    #[error("embedded SQL `{sql}` could not be parsed: {source}")]
    EmbeddedSql {
        /// The SQL as the parser saw it, one space between tokens.
        sql: String,
        /// The parser error.
        #[source]
        source: sqlparser::parser::ParserError,
    },

    /// `OPEN` names a cursor that no enclosing block binds to a query.
    #[error("cursor '{name}' is not bound to a query by any enclosing block")]
    UnboundCursor {
        /// The cursor name, each part folded as PostgreSQL folds it and
        /// joined by periods.
        name: String,
    },

    /// A `DECLARE` section declares a name twice, which PostgreSQL refuses.
    #[error("'{name}' is declared twice in one DECLARE section")]
    DuplicateDeclaration {
        /// The name, folded as PostgreSQL folds it.
        name: String,
    },

    /// `EXIT` without a label or `CONTINUE` stands outside every loop, or
    /// `CONTINUE` names a label whose innermost carrier around it is a block
    /// or that nothing around it carries, which PostgreSQL refuses.
    #[error(
        "{} {} reaches no enclosing loop",
        .kind.keyword(),
        .label.as_deref().unwrap_or("without a label")
    )]
    NoEnclosingLoop {
        /// Whether the statement is `EXIT` or `CONTINUE`.
        kind: ExitKind,
        /// The label it names, folded as PostgreSQL folds an identifier.
        label: Option<String>,
    },

    /// The body nests blocks and control structures deeper than the reader
    /// follows.
    #[error("PL/pgSQL body nests blocks and control structures deeper than {limit}")]
    NestingTooDeep {
        /// The deepest nesting followed.
        limit: usize,
    },
}

/// A block, `[DECLARE ...] BEGIN ... [EXCEPTION ...] END`.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    /// The `<<label>>` before the block, folded as PostgreSQL folds an
    /// identifier, which qualifies the names the block declares.
    pub label: Option<String>,
    /// Every name the `DECLARE` section binds, in the order written.
    ///
    /// Each default is evaluated in this order on entering the block, before
    /// the block's handlers take effect, so an error there reaches the
    /// enclosing block's handlers instead.
    pub declarations: Vec<Declaration>,
    /// The block's statements.
    pub statements: Vec<Node>,
    /// Every `WHEN ... THEN` of the `EXCEPTION` section, in the order
    /// written.
    ///
    /// A handler runs once a statement raises, after PostgreSQL has rolled back
    /// everything the block's statements did, `SET` included.
    pub handlers: Vec<Handler>,
}

/// One `WHEN condition [OR condition ...] THEN statements` of an `EXCEPTION`
/// section.
///
/// An error runs the first handler with a condition it matches. A condition
/// whose code names a class, ending in `000`, matches every error of the
/// class.
#[derive(Clone, Debug, PartialEq)]
pub struct Handler {
    /// The conditions the handler catches, in the order written.
    pub conditions: Vec<Condition>,
    /// The statements the handler runs.
    pub statements: Vec<Node>,
}

/// An error condition, as a [`Handler`] catches it or a [`Raise`] raises it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Condition {
    /// A condition name such as `unique_violation`, folded as PostgreSQL
    /// folds an identifier.
    ///
    /// A handler's `others` catches every error except `query_canceled` and
    /// `assert_failure`.
    Name(String),
    /// `SQLSTATE 'code'`, five digits or upper-case ASCII letters.
    Sqlstate(String),
}

/// A name a `DECLARE` section binds.
#[derive(Clone, Debug, PartialEq)]
pub struct Declaration {
    /// The name, folded as PostgreSQL folds an identifier.
    pub name: String,
    /// What the name denotes.
    pub kind: DeclarationKind,
}

/// What a declared name denotes.
#[derive(Clone, Debug, PartialEq)]
pub enum DeclarationKind {
    /// `name [CONSTANT] type [COLLATE collation] [NOT NULL] [default]`.
    Variable(Variable),
    /// `name ALIAS FOR target`, another name for a parameter or variable.
    Alias(AliasTarget),
    /// `name [[NO] SCROLL] CURSOR [(parameters)] FOR query`, a cursor bound
    /// to a query.
    ///
    /// An unbound cursor is a [`Variable`] of type `refcursor`.
    Cursor(Cursor),
}

/// A declared variable.
#[derive(Clone, Debug, PartialEq)]
pub struct Variable {
    /// Whether `CONSTANT` forbids assigning the variable.
    pub constant: bool,
    /// The type written for the variable.
    pub data_type: DeclaredType,
    /// The `COLLATE` collation.
    pub collation: Option<ObjectName>,
    /// Whether `NOT NULL` forbids assigning the variable a null.
    pub not_null: bool,
    /// The `SELECT` that evaluates the default written after `DEFAULT`, `:=`
    /// or `=`.
    pub default: Option<Box<Query>>,
}

/// The type a declaration writes.
#[derive(Clone, Debug, PartialEq)]
pub enum DeclaredType {
    /// A type named directly.
    Data(DataType),
    /// `name%TYPE`, the type of a column or variable, with `[]` for an array
    /// of it.
    ColumnType {
        /// The column or variable, as written.
        of: ObjectName,
        /// Whether `[]` follows.
        array: bool,
    },
    /// `name%ROWTYPE`, the row type of a table or view, with `[]` for an
    /// array of it.
    RowType {
        /// The table or view, as written.
        of: ObjectName,
        /// Whether `[]` follows.
        array: bool,
    },
}

/// What `ALIAS FOR` names.
#[derive(Clone, Debug, PartialEq)]
pub enum AliasTarget {
    /// `$n`, the function's parameter at position `n`, counting from 1.
    Parameter(usize),
    /// A parameter or variable, as written.
    Name(ObjectName),
}

/// A cursor declared with its query.
///
/// The query itself appears in each [`BoundCursor`] that opens the cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct Cursor {
    /// The parameters the query reads, bound when the cursor opens.
    pub parameters: Vec<CursorParameter>,
}

/// A parameter of a cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorParameter {
    /// The name, folded as PostgreSQL folds an identifier.
    pub name: String,
    /// The type written for the parameter.
    pub data_type: DeclaredType,
}

/// One step of a body.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// An SQL statement or query PostgreSQL runs at this point.
    Statement {
        /// The statement, without the `INTO` clause PL/pgSQL reads out of it.
        statement: Box<Statement>,
        /// The `INTO` clause, which assigns the statement's first row.
        into: Option<IntoTargets>,
    },
    /// `target := value`.
    Assignment(Assignment),
    /// SQL text the body builds at run time and executes.
    Dynamic {
        /// The command and its arguments.
        dynamic: Dynamic,
        /// The `INTO` clause of `EXECUTE`, which assigns the first row the
        /// command returns.
        into: Option<IntoTargets>,
    },
    /// `FETCH`, which assigns the row it reads.
    ///
    /// Any count its direction evaluates is the step before it.
    Fetch(Fetch),
    /// `GET [CURRENT | STACKED] DIAGNOSTICS`.
    Diagnostics(Diagnostics),
    /// `OPEN`, which assigns the cursor variable a generated portal name
    /// when it holds null.
    Open(Open),
    /// A nested block.
    Block(Block),
    /// An `IF` statement.
    If(If),
    /// A `CASE` statement.
    Case(Case),
    /// A loop of any kind.
    Loop(Loop),
    /// `EXIT` or `CONTINUE`.
    Exit(Exit),
    /// `RETURN`, which leaves the function.
    ///
    /// `RETURN NEXT` and `RETURN QUERY` add rows to the result and carry on,
    /// so they are a [`Node::Statement`] or a [`Node::Dynamic`].
    Return {
        /// The `SELECT` that evaluates the returned value, absent from the
        /// bare `RETURN` of a procedure or of a function returning `void`,
        /// a set or output parameters.
        value: Option<Box<Query>>,
    },
    /// `RAISE`, which reports a message or raises an error.
    Raise(Raise),
    /// `ASSERT`, which raises an error when its condition fails.
    Assert(Assert),
}

/// A cursor declared with its query, as `OPEN` or a `FOR` loop opens it.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCursor {
    /// The cursor, as written.
    pub name: ObjectName,
    /// The `SELECT` that evaluates each argument, a named argument's name
    /// dropped.
    pub arguments: Vec<Query>,
    /// The query the cursor's declaration binds.
    ///
    /// It reads the names in reach where the cursor is declared and the
    /// cursor's parameters, which [`crate::namespace`] answers.
    pub query: Box<Statement>,
}

/// A variable, or a part of one, that a statement assigns.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// The name up to the first subscript, as written. It holds the
    /// variable, any label qualifying it and any fields of it, which only a
    /// lookup tells apart.
    pub name: ObjectName,
    /// The subscripts after the name and the fields of the elements they
    /// pick, in the order written.
    ///
    /// Only an assignment writes these.
    pub indirection: Vec<Indirection>,
}

/// One step into the value a [`Target`] names.
#[derive(Clone, Debug, PartialEq)]
pub enum Indirection {
    /// `.field`, a field of a composite value.
    Field(Ident),
    /// `[index]`, as the `SELECT` that evaluates the index.
    Index(Box<Query>),
    /// `[lower:upper]`, each bound written as the `SELECT` that evaluates it.
    Slice {
        /// The first bound, unless omitted.
        lower: Option<Box<Query>>,
        /// The second bound, unless omitted.
        upper: Option<Box<Query>>,
    },
}

/// `target := value`, or `target = value`.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    /// What the value is assigned to.
    pub target: Target,
    /// The `SELECT` that evaluates the value.
    pub value: Box<Query>,
}

/// `INTO [STRICT] target, ...`.
#[derive(Clone, Debug, PartialEq)]
pub struct IntoTargets {
    /// Whether `STRICT` makes any row count but one an error.
    pub strict: bool,
    /// One row or record variable, or the variables receiving the columns
    /// in order.
    pub targets: Vec<Target>,
}

/// `FETCH [direction {FROM | IN}] cursor INTO target, ...`.
#[derive(Clone, Debug, PartialEq)]
pub struct Fetch {
    /// The cursor variable, as written.
    pub cursor: ObjectName,
    /// One row or record variable, or the variables receiving the columns
    /// in order.
    pub targets: Vec<Target>,
}

/// `GET [CURRENT | STACKED] DIAGNOSTICS target = item, ...`.
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostics {
    /// Whether `STACKED` reads the error a handler caught, instead of the
    /// statement run last.
    pub stacked: bool,
    /// Every `target = item`, in the order written.
    pub items: Vec<DiagnosticsItem>,
}

/// One `target = item` of `GET DIAGNOSTICS`.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticsItem {
    /// The variable the item is assigned to.
    pub target: Target,
    /// The item.
    pub kind: DiagnosticsKind,
}

/// What `GET DIAGNOSTICS` reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiagnosticsKind {
    /// `ROW_COUNT`, only without `STACKED`.
    RowCount,
    /// `PG_ROUTINE_OID`, only without `STACKED`.
    RoutineOid,
    /// `PG_CONTEXT`.
    Context,
    /// `RETURNED_SQLSTATE`, only with `STACKED`.
    ReturnedSqlstate,
    /// `COLUMN_NAME`, only with `STACKED`.
    ColumnName,
    /// `CONSTRAINT_NAME`, only with `STACKED`.
    ConstraintName,
    /// `PG_DATATYPE_NAME`, only with `STACKED`.
    DatatypeName,
    /// `MESSAGE_TEXT`, only with `STACKED`.
    MessageText,
    /// `TABLE_NAME`, only with `STACKED`.
    TableName,
    /// `SCHEMA_NAME`, only with `STACKED`.
    SchemaName,
    /// `PG_EXCEPTION_DETAIL`, only with `STACKED`.
    ExceptionDetail,
    /// `PG_EXCEPTION_HINT`, only with `STACKED`.
    ExceptionHint,
    /// `PG_EXCEPTION_CONTEXT`, only with `STACKED`.
    ExceptionContext,
}

impl DiagnosticsKind {
    const KEYWORDS: [(&str, Self); 13] = [
        ("ROW_COUNT", Self::RowCount),
        ("PG_ROUTINE_OID", Self::RoutineOid),
        ("PG_CONTEXT", Self::Context),
        ("RETURNED_SQLSTATE", Self::ReturnedSqlstate),
        ("COLUMN_NAME", Self::ColumnName),
        ("CONSTRAINT_NAME", Self::ConstraintName),
        ("PG_DATATYPE_NAME", Self::DatatypeName),
        ("MESSAGE_TEXT", Self::MessageText),
        ("TABLE_NAME", Self::TableName),
        ("SCHEMA_NAME", Self::SchemaName),
        ("PG_EXCEPTION_DETAIL", Self::ExceptionDetail),
        ("PG_EXCEPTION_HINT", Self::ExceptionHint),
        ("PG_EXCEPTION_CONTEXT", Self::ExceptionContext),
    ];

    /// Whether PostgreSQL accepts the item with `STACKED` when `stacked`
    /// holds, and without it otherwise.
    const fn allowed(self, stacked: bool) -> bool {
        match self {
            Self::RowCount | Self::RoutineOid => !stacked,
            Self::Context => true,
            _ => stacked,
        }
    }
}

/// `OPEN cursor ...`, by where the query it runs comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Open {
    /// `OPEN cursor [(arguments)]`, for a cursor declared with its query.
    Bound(BoundCursor),
    /// `OPEN cursor [[NO] SCROLL] FOR query`.
    Query {
        /// The cursor variable, as written.
        cursor: ObjectName,
        /// The query.
        query: Box<Statement>,
    },
    /// `OPEN cursor [[NO] SCROLL] FOR EXECUTE command [USING ...]`.
    Dynamic {
        /// The cursor variable, as written.
        cursor: ObjectName,
        /// The command and its arguments.
        dynamic: Dynamic,
    },
}

impl Open {
    /// The cursor variable, as written.
    #[must_use]
    pub const fn cursor(&self) -> &ObjectName {
        match self {
            Self::Bound(bound) => &bound.name,
            Self::Query { cursor, .. } | Self::Dynamic { cursor, .. } => cursor,
        }
    }
}

/// SQL built at run time by `EXECUTE`, `RETURN QUERY EXECUTE`,
/// `OPEN ... FOR EXECUTE` or `FOR ... IN EXECUTE`, which no reader can
/// resolve.
#[derive(Clone, Debug, PartialEq)]
pub struct Dynamic {
    /// The `SELECT` that evaluates the command string.
    pub command: Box<Query>,
    /// The `SELECT` that evaluates each `USING` argument.
    pub arguments: Vec<Query>,
}

impl Dynamic {
    /// The command's text when it is a string constant, single-quoted, `E''`
    /// or dollar-quoted.
    ///
    /// `None` for anything else, `NULL` included, which only running the body
    /// decides.
    #[must_use]
    pub fn constant_command(&self) -> Option<&str> {
        let Some(Expr::Value(value)) = lone_expression(&self.command) else {
            return None;
        };
        match &value.value {
            Value::SingleQuotedString(text) | Value::EscapedStringLiteral(text) => Some(text),
            Value::DollarQuotedString(text) => Some(&text.value),
            _ => None,
        }
    }
}

/// A condition and the statements that run when it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Branch {
    /// The `SELECT` that evaluates the condition.
    ///
    /// For a `WHEN` of a `CASE` with a subject, it selects the listed values
    /// the subject is compared with.
    pub condition: Box<Query>,
    /// The statements of the branch.
    pub statements: Vec<Node>,
}

/// `IF ... THEN ... [ELSIF ... THEN ...] [ELSE ...] END IF`.
#[derive(Clone, Debug, PartialEq)]
pub struct If {
    /// The `IF` branch and every `ELSIF`, tried in order until one holds.
    pub branches: Vec<Branch>,
    /// The `ELSE` statements.
    pub otherwise: Vec<Node>,
}

/// `CASE [subject] WHEN ... THEN ... [ELSE ...] END CASE`.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    /// The `SELECT` that evaluates the subject, when there is one.
    pub subject: Option<Box<Query>>,
    /// Every `WHEN`, tried in order until one holds.
    pub branches: Vec<Branch>,
    /// The `ELSE` statements.
    pub otherwise: Vec<Node>,
}

/// A loop and the statements it repeats.
#[derive(Clone, Debug, PartialEq)]
pub struct Loop {
    /// The `<<label>>` before the loop, folded as PostgreSQL folds an
    /// identifier, which qualifies the variable a `FOR` loop declares.
    pub label: Option<String>,
    /// What controls the loop.
    pub control: LoopControl,
    /// The statements each iteration runs.
    pub statements: Vec<Node>,
}

/// The SQL that controls a loop, all of which runs before the first
/// iteration.
#[derive(Clone, Debug, PartialEq)]
pub enum LoopControl {
    /// `LOOP`, which only `EXIT` or `RETURN` ends.
    Unconditional,
    /// `WHILE condition LOOP`, as the `SELECT` evaluated before each iteration.
    While(Box<Query>),
    /// `FOR i IN [REVERSE] lower .. upper [BY step] LOOP`, each bound as the
    /// `SELECT` evaluated once.
    Range {
        /// The integer variable the loop declares for its statements, which
        /// hides any other variable of that name.
        variable: String,
        /// Whether `REVERSE` counts down from the first bound to the second
        /// by the step, instead of up.
        reverse: bool,
        /// The first bound written, where the count starts.
        lower: Box<Query>,
        /// The second bound written, past which the count stops.
        upper: Box<Query>,
        /// The `BY` step.
        step: Option<Box<Query>>,
    },
    /// `FOR target IN query LOOP`.
    Query {
        /// One row or record variable, or the variables receiving the
        /// columns in order, each assigned every row in turn. The loop
        /// declares none of them.
        targets: Vec<Target>,
        /// The query.
        query: Box<Statement>,
    },
    /// `FOR target IN cursor [(arguments)] LOOP`.
    Cursor {
        /// The record variable the loop declares for its statements, which
        /// hides any other variable of that name.
        variable: String,
        /// The cursor the loop opens.
        cursor: BoundCursor,
    },
    /// `FOR target IN EXECUTE command [USING ...] LOOP`.
    Dynamic {
        /// One row or record variable, or the variables receiving the
        /// columns in order, each assigned every row in turn. The loop
        /// declares none of them.
        targets: Vec<Target>,
        /// The command and its arguments.
        dynamic: Dynamic,
    },
    /// `FOREACH target [SLICE n] IN ARRAY array LOOP`.
    Array {
        /// One variable, or the fields of a composite element in order,
        /// each assigned every element or slice in turn. The loop declares
        /// none of them.
        targets: Vec<Target>,
        /// The `SLICE` dimension, 0 without one, which assigns elements.
        slice: u32,
        /// The `SELECT` that evaluates the array.
        array: Box<Query>,
    },
}

/// `EXIT [label] [WHEN condition]` or `CONTINUE [label] [WHEN condition]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Exit {
    /// Whether it leaves or starts the next iteration.
    pub kind: ExitKind,
    /// The label it names, folded as PostgreSQL folds an identifier.
    ///
    /// It reaches the innermost enclosing block or loop that carries the
    /// label, always a loop for `CONTINUE`. In a body PostgreSQL compiles, a
    /// label no enclosing block or loop carries is the function's name,
    /// which labels the level holding the parameters, and `EXIT` naming it
    /// leaves the whole body. Without a label it reaches the innermost
    /// enclosing loop.
    pub label: Option<String>,
    /// The `SELECT` that evaluates the `WHEN` condition, without which it
    /// always transfers control.
    pub condition: Option<Box<Query>>,
}

/// Which of `EXIT` and `CONTINUE` an [`Exit`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExitKind {
    /// `EXIT`, which leaves the loop or block it reaches and carries on
    /// after it.
    Exit,
    /// `CONTINUE`, which skips the rest of the loop it reaches and starts
    /// its next iteration, if its control allows one.
    Continue,
}

impl ExitKind {
    /// The keyword the statement starts with.
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Exit => "EXIT",
            Self::Continue => "CONTINUE",
        }
    }
}

/// `RAISE [level] [format [, argument ...]] [USING option = value, ...]`,
/// with a condition name or `SQLSTATE` in place of the format, or bare.
///
/// One without a condition, a format or an option raises again the error a
/// handler caught, whatever its level.
#[derive(Clone, Debug, PartialEq)]
pub struct Raise {
    /// The level, `EXCEPTION` when none is written.
    pub level: RaiseLevel,
    /// The condition written in place of the format.
    pub condition: Option<Condition>,
    /// The format string's text, in which each `%` stands for the next
    /// argument.
    pub format: Option<String>,
    /// The `SELECT` that evaluates each argument the format string reads.
    pub arguments: Vec<Query>,
    /// Each `USING` option, in the order written.
    pub options: Vec<RaiseOption>,
}

/// One `option = value` after the `USING` of a [`Raise`].
#[derive(Clone, Debug, PartialEq)]
pub struct RaiseOption {
    /// The option.
    pub kind: RaiseOptionKind,
    /// The `SELECT` that evaluates its value.
    pub value: Query,
}

/// What a [`RaiseOption`] sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RaiseOptionKind {
    /// `ERRCODE`, a condition name or an SQLSTATE code.
    Errcode,
    /// `MESSAGE`.
    Message,
    /// `DETAIL`.
    Detail,
    /// `HINT`.
    Hint,
    /// `COLUMN`.
    Column,
    /// `CONSTRAINT`.
    Constraint,
    /// `DATATYPE`.
    Datatype,
    /// `TABLE`.
    Table,
    /// `SCHEMA`.
    Schema,
}

impl RaiseOptionKind {
    const KEYWORDS: [(&str, Self); 9] = [
        ("ERRCODE", Self::Errcode),
        ("MESSAGE", Self::Message),
        ("DETAIL", Self::Detail),
        ("HINT", Self::Hint),
        ("COLUMN", Self::Column),
        ("CONSTRAINT", Self::Constraint),
        ("DATATYPE", Self::Datatype),
        ("TABLE", Self::Table),
        ("SCHEMA", Self::Schema),
    ];
}

/// The level of a [`Raise`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RaiseLevel {
    /// `DEBUG`.
    Debug,
    /// `LOG`.
    Log,
    /// `INFO`.
    Info,
    /// `NOTICE`.
    Notice,
    /// `WARNING`.
    Warning,
    /// `EXCEPTION`, the one level that raises an error, which leaves for the
    /// handlers of the enclosing blocks. Every other level reports a message
    /// and carries on.
    Exception,
}

/// `ASSERT condition [, message]`.
///
/// When assertions are enabled, as `plpgsql.check_asserts` makes them by
/// default, a condition that is false or null raises `assert_failure`, which
/// leaves for the handlers of the enclosing blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct Assert {
    /// The `SELECT` that evaluates the condition.
    pub condition: Box<Query>,
    /// The `SELECT` that evaluates the message, which only a failing
    /// condition evaluates.
    pub message: Option<Box<Query>>,
}

/// Reads every SQL statement and expression a PL/pgSQL body runs.
///
/// `body` is the function's source between its dollar quotes. A cursor's
/// query appears where the cursor opens, since that is when PostgreSQL plans
/// it.
///
/// # Errors
///
/// Returns an [`Error`] when the body is not PL/pgSQL, when an SQL statement
/// or expression in it does not parse, when a `DECLARE` section declares a
/// name twice, when `OPEN` names a cursor no enclosing block binds, or when
/// it nests deeper than the reader follows.
///
/// # Example
///
/// ```
/// use sqlparser_plpgsql::{embedded::Node, embedded_sql};
///
/// let block = embedded_sql(
///     "BEGIN PERFORM 1 FROM docs; s := 'DELETE FROM ' || t; EXECUTE s; END",
/// )?;
/// let Node::Statement { statement, .. } = &block.statements[0] else { unreachable!() };
/// assert_eq!(statement.to_string(), "SELECT 1 FROM docs");
/// let Node::Assignment(assignment) = &block.statements[1] else { unreachable!() };
/// assert_eq!(assignment.target.name.to_string(), "s");
/// assert_eq!(assignment.value.to_string(), "SELECT 'DELETE FROM ' || t");
/// assert!(matches!(block.statements[2], Node::Dynamic { .. }));
/// # Ok::<(), sqlparser_plpgsql::embedded::Error>(())
/// ```
pub fn embedded_sql(body: &str) -> Result<Block, Error> {
    let text = spaced_ranges(body);
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, &text)
        .tokenize()
        .map_err(|source| Error::UntokenizableBody { source })?
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect();
    let mut reader = Reader {
        tokens,
        index: 0,
        depth: 0,
        level: None,
        enclosing: Vec::new(),
    };

    reader.compiler_options()?;
    let label = reader.label()?;
    let block = reader.nested(|reader| reader.block(label))?;
    reader.eat_token(&Token::SemiColon);
    if reader.peek().is_some() {
        return Err(reader.syntax("the end of the body"));
    }
    Ok(block)
}

/// The expression a `SELECT` read from a PL/pgSQL expression evaluates.
///
/// That is the one expression it selects, or the whole query as a scalar
/// subquery when it writes more, a `FROM` for one.
///
/// # Example
///
/// ```
/// use sqlparser_plpgsql::{embedded::{self, Node}, embedded_sql};
///
/// let block = embedded_sql(
///     "BEGIN EXECUTE 'SELECT ' || t; EXECUTE c FROM commands() AS c; END",
/// )?;
/// let Node::Dynamic { dynamic: built, .. } = &block.statements[0] else { unreachable!() };
/// assert_eq!(embedded::expression(&built.command).to_string(), "'SELECT ' || t");
/// let Node::Dynamic { dynamic: read, .. } = &block.statements[1] else { unreachable!() };
/// assert_eq!(
///     embedded::expression(&read.command).to_string(),
///     "(SELECT c FROM commands() AS c)",
/// );
/// # Ok::<(), sqlparser_plpgsql::embedded::Error>(())
/// ```
#[must_use]
pub fn expression(select: &Query) -> Cow<'_, Expr> {
    lone_expression(select).map_or_else(
        || Cow::Owned(Expr::Subquery(Box::new(select.clone()))),
        Cow::Borrowed,
    )
}

/// The one expression `select` selects, when it writes nothing else.
fn lone_expression(select: &Query) -> Option<&Expr> {
    let Query {
        with: None,
        body,
        order_by: None,
        limit_clause: None,
        fetch: None,
        locks,
        for_clause: None,
        settings: None,
        format_clause: None,
        pipe_operators,
        ..
    } = select
    else {
        return None;
    };
    let SetExpr::Select(body) = &**body else {
        return None;
    };
    let Select {
        distinct: None,
        top: None,
        into: None,
        prewhere: None,
        selection: None,
        having: None,
        qualify: None,
        value_table_mode: None,
        exclude: None,
        select_modifiers: None,
        optimizer_hints,
        projection,
        from,
        lateral_views,
        connect_by,
        group_by: GroupByExpr::Expressions(keys, modifiers),
        cluster_by,
        distribute_by,
        sort_by,
        named_window,
        ..
    } = &**body
    else {
        return None;
    };
    let [SelectItem::UnnamedExpr(expression)] = projection.as_slice() else {
        return None;
    };
    let alone = locks.is_empty()
        && pipe_operators.is_empty()
        && optimizer_hints.is_empty()
        && from.is_empty()
        && lateral_views.is_empty()
        && connect_by.is_empty()
        && keys.is_empty()
        && modifiers.is_empty()
        && cluster_by.is_empty()
        && distribute_by.is_empty()
        && sort_by.is_empty()
        && named_window.is_empty();
    alone.then_some(expression)
}

/// Sets every `..` of live code apart from its neighbours.
///
/// The tokenizer reads `1..10` as the numbers `1.` and `.10`, which would
/// hide the range of an integer `FOR` loop.
fn spaced_ranges(body: &str) -> Cow<'_, str> {
    let regions = Scanner::new(body).regions();
    let bytes = body.as_bytes();
    let live = |index: usize| regions.get(index) == Some(&Region::Code);
    let mut spaced = String::new();
    let mut copied = 0;
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'.' && bytes[index + 1] == b'.' && live(index) {
            spaced.push_str(&body[copied..index]);
            spaced.push_str(" .. ");
            index += 2;
            copied = index;
        } else {
            index += 1;
        }
    }
    if copied == 0 {
        return Cow::Borrowed(body);
    }
    spaced.push_str(&body[copied..]);
    Cow::Owned(spaced)
}

/// The PL/pgSQL statements recognised by their first word.
#[derive(Clone, Copy)]
enum Command {
    Block,
    If,
    Case,
    Loop,
    While,
    For,
    Foreach,
    Exit(ExitKind),
    Return,
    Raise,
    Assert,
    Execute,
    Perform,
    Open,
    Fetch,
    Diagnostics,
    Silent,
}

impl Command {
    /// The statement `word` opens, when it is a PL/pgSQL keyword rather than
    /// the start of SQL or of an assignment.
    fn of(word: &Word) -> Option<Self> {
        const COMMANDS: [(&str, Command); 23] = [
            ("DECLARE", Command::Block),
            ("BEGIN", Command::Block),
            ("IF", Command::If),
            ("CASE", Command::Case),
            ("LOOP", Command::Loop),
            ("WHILE", Command::While),
            ("FOR", Command::For),
            ("FOREACH", Command::Foreach),
            ("EXIT", Command::Exit(ExitKind::Exit)),
            ("CONTINUE", Command::Exit(ExitKind::Continue)),
            ("RETURN", Command::Return),
            ("RAISE", Command::Raise),
            ("ASSERT", Command::Assert),
            ("EXECUTE", Command::Execute),
            ("PERFORM", Command::Perform),
            ("OPEN", Command::Open),
            ("FETCH", Command::Fetch),
            ("MOVE", Command::Fetch),
            ("GET", Command::Diagnostics),
            ("CLOSE", Command::Silent),
            ("NULL", Command::Silent),
            ("COMMIT", Command::Silent),
            ("ROLLBACK", Command::Silent),
        ];
        if word.quote_style.is_some() {
            return None;
        }
        COMMANDS
            .iter()
            .find(|(name, _)| word.value.eq_ignore_ascii_case(name))
            .map(|&(_, command)| command)
    }
}

/// Where a run of SQL tokens ends.
#[derive(Clone, Copy)]
enum End {
    Semicolon,
    Loop,
}

impl End {
    fn at(self, token: &Token) -> bool {
        match self {
            End::Semicolon => *token == Token::SemiColon,
            End::Loop => is_keyword(token, "LOOP"),
        }
    }
}

/// The query a name binds when it is a cursor declared with one, as the
/// reader's levels hold it.
type Bound = Option<Arc<Statement>>;

/// A recursive-descent walk over the body's tokens.
struct Reader {
    tokens: Vec<Token>,
    index: usize,
    depth: usize,
    /// The innermost level of names, where `OPEN` and `FOR` find a cursor.
    level: Option<Level<'static, Bound>>,
    /// Every block and loop around the next statement, innermost last,
    /// where `EXIT` and `CONTINUE` find what they reach.
    enclosing: Vec<Enclosing>,
}

/// A block or loop around the statements the reader reads.
struct Enclosing {
    /// The label, folded as PostgreSQL folds an identifier.
    label: Option<String>,
    /// Whether it is a loop, which alone `CONTINUE` reaches.
    looped: bool,
}

impl Reader {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn peek_nth(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.index + offset)
    }

    fn at(&self, keyword: &str) -> bool {
        self.peek().is_some_and(|token| is_keyword(token, keyword))
    }

    fn at_any(&self, keywords: &[&str]) -> bool {
        keywords.iter().any(|keyword| self.at(keyword))
    }

    fn eat(&mut self, keyword: &str) -> bool {
        let found = self.at(keyword);
        if found {
            self.index += 1;
        }
        found
    }

    fn eat_token(&mut self, token: &Token) -> bool {
        let found = self.peek() == Some(token);
        if found {
            self.index += 1;
        }
        found
    }

    fn expect(&mut self, keyword: &'static str) -> Result<(), Error> {
        if self.eat(keyword) {
            Ok(())
        } else {
            Err(self.syntax(keyword))
        }
    }

    fn expect_token(&mut self, token: &Token, expected: &'static str) -> Result<(), Error> {
        if self.eat_token(token) {
            Ok(())
        } else {
            Err(self.syntax(expected))
        }
    }

    fn expect_semicolon(&mut self) -> Result<(), Error> {
        self.expect_token(&Token::SemiColon, "`;`")
    }

    fn syntax(&self, expected: &'static str) -> Error {
        Error::Syntax {
            expected,
            found: self
                .peek()
                .map_or_else(|| "the end of the body".into(), ToString::to_string),
        }
    }

    /// Runs `read` one nesting level deeper, refusing past the limit.
    fn nested<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        if self.depth >= NESTING_LIMIT {
            return Err(Error::NestingTooDeep {
                limit: NESTING_LIMIT,
            });
        }
        self.depth += 1;
        let read = read(self);
        self.depth -= 1;
        read
    }

    /// Advances to the first token at bracket depth zero that `stop` accepts,
    /// returning the range passed over and leaving the stop token in place.
    ///
    /// Only brackets nest, as in PostgreSQL. Any other `;` is an error, which
    /// keeps a missing keyword from swallowing the statements after it.
    fn segment(
        &mut self,
        expected: &'static str,
        stop: impl Fn(&Token, Option<&Token>) -> bool,
    ) -> Result<Range<usize>, Error> {
        let start = self.index;
        let mut depth = 0_usize;
        loop {
            let Some(token) = self.peek() else {
                return Err(self.syntax(expected));
            };
            if depth == 0 && stop(token, self.peek_nth(1)) {
                return Ok(start..self.index);
            }
            match token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket if depth > 0 => depth -= 1,
                Token::RParen | Token::RBracket | Token::SemiColon => {
                    return Err(self.syntax(expected));
                }
                _ => {}
            }
            self.index += 1;
        }
    }

    /// Like [`Self::segment`], refusing a segment without tokens.
    fn filled_segment(
        &mut self,
        expected: &'static str,
        stop: impl Fn(&Token, Option<&Token>) -> bool,
    ) -> Result<Range<usize>, Error> {
        let range = self.segment(expected, stop)?;
        if range.is_empty() {
            return Err(self.syntax(expected));
        }
        Ok(range)
    }

    /// Reads up to the first token `stop` accepts as the `SELECT` that
    /// evaluates an expression.
    fn expression(
        &mut self,
        expected: &'static str,
        stop: impl Fn(&Token, Option<&Token>) -> bool,
    ) -> Result<Box<Query>, Error> {
        let range = self.segment(expected, stop)?;
        select(&self.tokens[range])
    }

    /// Reads up to `keyword` as the `SELECT` that evaluates an expression.
    fn expression_until(&mut self, keyword: &'static str) -> Result<Box<Query>, Error> {
        self.expression(keyword, |token, _| is_keyword(token, keyword))
    }

    /// Reads up to the `;` that ends the statement as the `SELECT` that
    /// evaluates an expression.
    fn expression_until_semicolon(&mut self) -> Result<Box<Query>, Error> {
        self.expression("`;`", |token, _| *token == Token::SemiColon)
    }

    /// Reads up to the `;` that ends the statement as one SQL statement.
    fn statement_until_semicolon(&mut self) -> Result<Box<Statement>, Error> {
        let range = self.segment("`;`", |token, _| *token == Token::SemiColon)?;
        parse(&self.tokens[range])
    }

    /// Skips `#option` lines, which configure the PL/pgSQL compiler.
    fn compiler_options(&mut self) -> Result<(), Error> {
        while self.eat_token(&Token::Sharp) {
            for _ in 0..2 {
                if !matches!(self.peek(), Some(Token::Word(_))) {
                    return Err(self.syntax("a compiler option"));
                }
                self.index += 1;
            }
        }
        Ok(())
    }

    /// Reads a `<<label>>`, when there is one.
    fn label(&mut self) -> Result<Option<String>, Error> {
        if !self.eat_token(&Token::ShiftLeft) {
            return Ok(None);
        }
        let label = self.name("a label")?;
        self.expect_token(&Token::ShiftRight, "`>>`")?;
        Ok(Some(label))
    }

    /// Skips the label an `END` may repeat.
    fn end_label(&mut self) {
        if matches!(self.peek(), Some(Token::Word(_))) {
            self.index += 1;
        }
    }

    /// Reads an identifier, folded the way PostgreSQL folds an unquoted one.
    fn name(&mut self, expected: &'static str) -> Result<String, Error> {
        let Some(Token::Word(word)) = self.peek() else {
            return Err(self.syntax(expected));
        };
        let name = identifier(word);
        self.index += 1;
        Ok(name)
    }

    /// The name starting at the next token, or a parameter's `$n`, as
    /// written with any fields after periods, with how many tokens it spans.
    fn peek_qualified(&self) -> Option<(ObjectName, usize)> {
        let first = match self.peek()? {
            Token::Word(first) => first.to_ident(Span::empty()),
            Token::Placeholder(placeholder) if parameter_number(placeholder).is_some() => {
                Ident::new(placeholder.clone())
            }
            _ => return None,
        };
        let mut parts = Vec::from([first]);
        let mut length = 1;
        while let (Some(Token::Period), Some(Token::Word(word))) =
            (self.peek_nth(length), self.peek_nth(length + 1))
        {
            parts.push(word.to_ident(Span::empty()));
            length += 2;
        }
        Some((ObjectName::from(parts), length))
    }

    /// The query of the cursor `name` reaches.
    ///
    /// Every variable counts as holding no row, which answers as PostgreSQL
    /// does for every body it compiles. Where PostgreSQL stops at a row
    /// instead, the name reaches no cursor and the body does not compile.
    fn bound_cursor(&self, name: &ObjectName) -> Option<&Statement> {
        self.level
            .as_ref()?
            .lookup(&stored_parts(name), &|_| false)
            .and_then(|(bound, _)| bound.as_deref())
    }

    /// A level inside the current one, labelled `label`.
    fn inner_level(&self, label: Option<&str>) -> Level<'static, Bound> {
        Level::new(
            label.map(|label| Cow::Owned(label.into())),
            self.level.as_ref(),
        )
    }

    /// Runs `read` with `level` as the innermost level of names.
    fn within<T>(
        &mut self,
        level: Level<'static, Bound>,
        read: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let enclosing = self.level.replace(level);
        let read = read(self);
        self.level = enclosing;
        read
    }

    fn block(&mut self, label: Option<String>) -> Result<Block, Error> {
        let mut level = self.inner_level(label.as_deref());
        let mut declarations = Vec::new();
        if self.eat("DECLARE") {
            let mut declared = BTreeSet::new();
            while !self.at("BEGIN") {
                // PostgreSQL accepts a repeated DECLARE inside the section.
                if !self.eat("DECLARE") {
                    declarations.push(self.declaration(&mut level, &mut declared)?);
                }
            }
        }
        self.expect("BEGIN")?;
        self.enclosing.push(Enclosing {
            label,
            looped: false,
        });
        let read = self.within(level.clone(), |reader| {
            let statements = reader.statements(&["EXCEPTION", "END"])?;
            if !reader.eat("EXCEPTION") {
                return Ok((statements, Vec::new()));
            }
            // The section declares SQLSTATE and SQLERRM in the block's level.
            let mut section = level;
            section.declare("sqlstate", None);
            section.declare("sqlerrm", None);
            let handlers = reader.within(section, |reader| {
                let mut handlers = Vec::new();
                loop {
                    reader.expect("WHEN")?;
                    let mut conditions = Vec::from([reader.handler_condition()?]);
                    while reader.eat("OR") {
                        conditions.push(reader.handler_condition()?);
                    }
                    reader.expect("THEN")?;
                    handlers.push(Handler {
                        conditions,
                        statements: reader.statements(&["WHEN", "END"])?,
                    });
                    if !reader.at("WHEN") {
                        return Ok(handlers);
                    }
                }
            })?;
            Ok((statements, handlers))
        });
        let label = self.enclosing.pop().and_then(|enclosing| enclosing.label);
        let (statements, handlers) = read?;
        self.expect("END")?;
        self.end_label();
        Ok(Block {
            label,
            declarations,
            statements,
            handlers,
        })
    }

    /// Reads one declaration into `level`, the level its block opens, where
    /// `declared` holds every name declared before it.
    fn declaration(
        &mut self,
        level: &mut Level<'static, Bound>,
        declared: &mut BTreeSet<String>,
    ) -> Result<Declaration, Error> {
        let name = self.name("a declaration")?;
        if !declared.insert(name.clone()) {
            return Err(Error::DuplicateDeclaration { name });
        }
        let cursor_words = [
            &["CURSOR"][..],
            &["SCROLL", "CURSOR"],
            &["NO", "SCROLL", "CURSOR"],
        ]
        .into_iter()
        .find(|words| {
            words.iter().enumerate().all(|(offset, word)| {
                self.peek_nth(offset)
                    .is_some_and(|token| is_keyword(token, word))
            })
        });

        let (kind, bound) = if self.eat("ALIAS") {
            self.expect("FOR")?;
            let target = self.alias_target()?;
            // An alias of a cursor opens the cursor's query. A parameter is
            // never a cursor declared with one.
            let bound = match &target {
                AliasTarget::Parameter(_) => None,
                AliasTarget::Name(name) => level
                    .lookup(&stored_parts(name), &|_| false)
                    .and_then(|(bound, _)| bound.clone()),
            };
            (DeclarationKind::Alias(target), bound)
        } else if let Some(words) = cursor_words {
            self.index += words.len();
            let parameters = if self.eat_token(&Token::LParen) {
                self.cursor_parameters()?
            } else {
                Vec::new()
            };
            if !(self.eat("FOR") || self.eat("IS")) {
                return Err(self.syntax("FOR or IS"));
            }
            let query = self.statement_until_semicolon()?;
            (
                DeclarationKind::Cursor(Cursor { parameters }),
                Some(Arc::from(query)),
            )
        } else {
            (DeclarationKind::Variable(self.variable()?), None)
        };
        self.expect_semicolon()?;
        level.declare(name.clone(), bound);
        Ok(Declaration { name, kind })
    }

    /// Reads what follows `ALIAS FOR`.
    fn alias_target(&mut self) -> Result<AliasTarget, Error> {
        if let Some(Token::Placeholder(placeholder)) = self.peek()
            && let Some(position) = parameter_number(placeholder)
        {
            self.index += 1;
            return Ok(AliasTarget::Parameter(position));
        }
        self.object_name("a parameter or variable", |token, _| {
            *token == Token::SemiColon
        })
        .map(AliasTarget::Name)
    }

    /// Reads up to the first token `stop` accepts as a qualified name.
    fn object_name(
        &mut self,
        expected: &'static str,
        stop: impl Fn(&Token, Option<&Token>) -> bool,
    ) -> Result<ObjectName, Error> {
        let range = self.filled_segment(expected, stop)?;
        parse_all(&self.tokens[range], false, |parser| {
            parser.parse_object_name(false)
        })
    }

    /// Reads the parameters of a cursor declaration after its `(`, through
    /// the `)`.
    fn cursor_parameters(&mut self) -> Result<Vec<CursorParameter>, Error> {
        let mut parameters = Vec::new();
        loop {
            let name = self.name("a cursor parameter")?;
            let data_type =
                self.declared_type(|token, _| matches!(token, Token::Comma | Token::RParen))?;
            parameters.push(CursorParameter { name, data_type });
            if !self.eat_token(&Token::Comma) {
                break;
            }
        }
        self.expect_token(&Token::RParen, "`)`")?;
        Ok(parameters)
    }

    /// Reads a variable declaration after its name, up to its `;`.
    fn variable(&mut self) -> Result<Variable, Error> {
        /// True where the type or the collation ends.
        fn ends_part(token: &Token, next: Option<&Token>) -> bool {
            matches!(token, Token::SemiColon | Token::Assignment | Token::Eq)
                || is_keyword(token, "DEFAULT")
                || is_keyword(token, "COLLATE")
                || (is_keyword(token, "NOT") && next.is_some_and(|next| is_keyword(next, "NULL")))
        }

        let constant = self.eat("CONSTANT");
        let data_type = self.declared_type(ends_part)?;
        let collation = if self.eat("COLLATE") {
            Some(self.object_name("a collation", ends_part)?)
        } else {
            None
        };
        let not_null = self.eat("NOT");
        if not_null {
            self.expect("NULL")?;
        }
        let default = if self.eat("DEFAULT")
            || self.eat_token(&Token::Assignment)
            || self.eat_token(&Token::Eq)
        {
            Some(self.expression_until_semicolon()?)
        } else {
            None
        };
        Ok(Variable {
            constant,
            data_type,
            collation,
            not_null,
            default,
        })
    }

    /// Reads the type a declaration writes, up to the first token `stop`
    /// accepts.
    fn declared_type(
        &mut self,
        stop: impl Fn(&Token, Option<&Token>) -> bool,
    ) -> Result<DeclaredType, Error> {
        let range = self.filled_segment("a data type", stop)?;
        declared_type(&self.tokens[range])
    }

    /// Reads statements until one of `until` starts the next.
    fn statements(&mut self, until: &[&str]) -> Result<Vec<Node>, Error> {
        let mut nodes = Vec::new();
        while !self.at_any(until) {
            self.statement(&mut nodes)?;
        }
        Ok(nodes)
    }

    fn statement(&mut self, nodes: &mut Vec<Node>) -> Result<(), Error> {
        let label = self.label()?;
        // PostgreSQL takes a statement's first word for a variable when an
        // assignment follows it, even an unreserved keyword such as `get`.
        if label.is_none() && self.at_assignment() {
            nodes.push(Node::Assignment(self.assignment()?));
            return self.expect_semicolon();
        }
        let Some(Token::Word(word)) = self.peek() else {
            return Err(self.syntax("a statement"));
        };
        let command = Command::of(word);
        if label.is_some()
            && !matches!(
                command,
                Some(
                    Command::Block
                        | Command::Loop
                        | Command::While
                        | Command::For
                        | Command::Foreach
                )
            )
        {
            return Err(self.syntax("a block or a loop after the label"));
        }

        match command {
            Some(Command::Block) => {
                let block = self.nested(|reader| reader.block(label))?;
                nodes.push(Node::Block(block));
            }
            Some(Command::If) => {
                let conditional = self.nested(Self::if_statement)?;
                nodes.push(Node::If(conditional));
            }
            Some(Command::Case) => {
                let case = self.nested(Self::case_statement)?;
                nodes.push(Node::Case(case));
            }
            Some(Command::Loop | Command::While | Command::For | Command::Foreach) => {
                let looped = self.nested(|reader| reader.loop_statement(label))?;
                nodes.push(Node::Loop(looped));
            }
            Some(Command::Exit(kind)) => nodes.push(Node::Exit(self.exit(kind)?)),
            Some(Command::Return) => nodes.extend(self.return_statement()?),
            Some(Command::Raise) => nodes.push(Node::Raise(self.raise()?)),
            Some(Command::Assert) => nodes.push(Node::Assert(self.assert()?)),
            Some(Command::Execute) => {
                self.index += 1;
                let (dynamic, into) = self.dynamic(End::Semicolon, true)?;
                nodes.push(Node::Dynamic { dynamic, into });
            }
            Some(Command::Perform) => {
                self.index += 1;
                nodes.push(evaluated(self.expression_until_semicolon()?));
            }
            Some(Command::Open) => nodes.push(Node::Open(self.open()?)),
            Some(Command::Fetch) => self.fetch(nodes)?,
            Some(Command::Diagnostics) => nodes.push(Node::Diagnostics(self.diagnostics()?)),
            Some(Command::Silent) => {
                self.segment("`;`", |token, _| *token == Token::SemiColon)?;
            }
            None => {
                let (statement, into) = self.sql_statement()?;
                nodes.push(Node::Statement { statement, into });
            }
        }
        self.expect_semicolon()
    }

    fn if_statement(&mut self) -> Result<If, Error> {
        self.index += 1;
        let mut branches = Vec::new();
        loop {
            let condition = self.expression_until("THEN")?;
            self.index += 1;
            let statements = self.statements(&["ELSIF", "ELSEIF", "ELSE", "END"])?;
            branches.push(Branch {
                condition,
                statements,
            });
            if !(self.eat("ELSIF") || self.eat("ELSEIF")) {
                break;
            }
        }
        let otherwise = self.otherwise()?;
        self.expect("IF")?;
        Ok(If {
            branches,
            otherwise,
        })
    }

    fn case_statement(&mut self) -> Result<Case, Error> {
        self.index += 1;
        let subject = if self.at("WHEN") {
            None
        } else {
            Some(self.expression_until("WHEN")?)
        };
        let mut branches = Vec::new();
        loop {
            self.expect("WHEN")?;
            let condition = self.expression_until("THEN")?;
            self.index += 1;
            let statements = self.statements(&["WHEN", "ELSE", "END"])?;
            branches.push(Branch {
                condition,
                statements,
            });
            if !self.at("WHEN") {
                break;
            }
        }
        let otherwise = self.otherwise()?;
        self.expect("CASE")?;
        Ok(Case {
            subject,
            branches,
            otherwise,
        })
    }

    /// Reads an optional `ELSE` and the `END` after it.
    fn otherwise(&mut self) -> Result<Vec<Node>, Error> {
        let otherwise = if self.eat("ELSE") {
            self.statements(&["END"])?
        } else {
            Vec::new()
        };
        self.expect("END")?;
        Ok(otherwise)
    }

    fn loop_statement(&mut self, label: Option<String>) -> Result<Loop, Error> {
        let control = if self.at("LOOP") {
            LoopControl::Unconditional
        } else if self.eat("WHILE") {
            LoopControl::While(self.expression_until("LOOP")?)
        } else if self.eat("FOR") {
            self.for_control()?
        } else {
            self.expect("FOREACH")?;
            let targets = self.targets("a loop target")?;
            let slice = if self.eat("SLICE") {
                let slice = match self.peek() {
                    Some(Token::Number(digits, false)) => digits.parse().ok(),
                    _ => None,
                }
                .ok_or_else(|| self.syntax("a slice dimension"))?;
                self.index += 1;
                slice
            } else {
                0
            };
            self.expect("IN")?;
            self.expect("ARRAY")?;
            LoopControl::Array {
                targets,
                slice,
                array: self.expression_until("LOOP")?,
            }
        };
        self.expect("LOOP")?;
        // The loop opens a level, which holds the variable a range or cursor
        // loop declares.
        let mut level = self.inner_level(label.as_deref());
        if let LoopControl::Range { variable, .. } | LoopControl::Cursor { variable, .. } = &control
        {
            level.declare(variable.clone(), None);
        }
        self.enclosing.push(Enclosing {
            label,
            looped: true,
        });
        let statements = self.within(level, |reader| reader.statements(&["END"]));
        let label = self.enclosing.pop().and_then(|enclosing| enclosing.label);
        let statements = statements?;
        self.expect("END")?;
        self.expect("LOOP")?;
        self.end_label();
        Ok(Loop {
            label,
            control,
            statements,
        })
    }

    fn for_control(&mut self) -> Result<LoopControl, Error> {
        let targets = self.targets("a loop target")?;
        self.expect("IN")?;
        if self.eat("EXECUTE") {
            return Ok(LoopControl::Dynamic {
                targets,
                dynamic: self.dynamic(End::Loop, false)?.0,
            });
        }
        let reverse = self.eat("REVERSE");
        if !reverse
            && let Some((name, length)) = self.peek_qualified()
            && (matches!(self.peek_nth(length), Some(Token::LParen))
                || self
                    .peek_nth(length)
                    .is_some_and(|token| is_keyword(token, "LOOP")))
            && let Some(query) = self.bound_cursor(&name).cloned().map(Box::new)
        {
            self.index += length;
            let arguments = if self.eat_token(&Token::LParen) {
                self.cursor_arguments()?
            } else {
                Vec::new()
            };
            return Ok(LoopControl::Cursor {
                variable: loop_variable(&targets)?,
                cursor: BoundCursor {
                    name,
                    arguments,
                    query,
                },
            });
        }

        let first = self.segment("LOOP", |token, next| {
            is_keyword(token, "LOOP") || is_range_dots(token, next)
        })?;
        if self.at("LOOP") {
            if reverse {
                return Err(self.syntax("`..`"));
            }
            return Ok(LoopControl::Query {
                targets,
                query: parse(&self.tokens[first])?,
            });
        }
        let lower = select(&self.tokens[first])?;
        self.index += 2;
        let upper = self.expression("LOOP", |token, _| {
            is_keyword(token, "BY") || is_keyword(token, "LOOP")
        })?;
        let step = if self.eat("BY") {
            Some(self.expression_until("LOOP")?)
        } else {
            None
        };
        Ok(LoopControl::Range {
            variable: loop_variable(&targets)?,
            reverse,
            lower,
            upper,
            step,
        })
    }

    /// Reads the arguments of a cursor after its `(`, through the `)`.
    ///
    /// A named argument's `name :=` or `name =>` is dropped, since only the
    /// value is evaluated.
    fn cursor_arguments(&mut self) -> Result<Vec<Query>, Error> {
        let mut arguments = Vec::new();
        loop {
            if matches!(self.peek(), Some(Token::Word(_)))
                && matches!(self.peek_nth(1), Some(Token::Assignment | Token::RArrow))
            {
                self.index += 2;
            }
            arguments.push(*self.expression("`)`", |token, _| {
                matches!(token, Token::Comma | Token::RParen)
            })?);
            if !self.eat_token(&Token::Comma) {
                break;
            }
        }
        self.expect_token(&Token::RParen, "`)`")?;
        Ok(arguments)
    }

    /// Reads a command string and its `USING` clause up to `end`, with an
    /// `INTO` clause in either order where `into` allows one, as only the
    /// `EXECUTE` statement does.
    fn dynamic(&mut self, end: End, into: bool) -> Result<(Dynamic, Option<IntoTargets>), Error> {
        let clause_or_end = move |token: &Token| {
            end.at(token) || is_keyword(token, "INTO") || is_keyword(token, "USING")
        };
        let command = self.expression("the end of the command", |token, _| clause_or_end(token))?;
        let mut arguments = Vec::new();
        let mut targets = None;
        loop {
            if into && targets.is_none() && self.eat("INTO") {
                targets = Some(self.read_into()?);
            } else if self.eat("USING") {
                loop {
                    arguments.push(*self.expression("the end of the command", |token, _| {
                        *token == Token::Comma || clause_or_end(token)
                    })?);
                    if !self.eat_token(&Token::Comma) {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        Ok((Dynamic { command, arguments }, targets))
    }

    /// Reads `[STRICT] target, ...` after an `INTO`.
    fn read_into(&mut self) -> Result<IntoTargets, Error> {
        let strict = self.eat("STRICT");
        Ok(IntoTargets {
            strict,
            targets: self.targets("an INTO target")?,
        })
    }

    /// Reads `target, ...`, each a name with any fields, as `INTO`, `FETCH`,
    /// `FOR` and `FOREACH` write them.
    fn targets(&mut self, expected: &'static str) -> Result<Vec<Target>, Error> {
        let mut targets = Vec::from([self.plain_target(expected)?]);
        while self.peek() == Some(&Token::Comma)
            && matches!(
                self.peek_nth(1),
                Some(Token::Word(_) | Token::Placeholder(_))
            )
        {
            self.index += 1;
            targets.push(self.plain_target(expected)?);
        }
        Ok(targets)
    }

    /// Reads a target without subscripts.
    fn plain_target(&mut self, expected: &'static str) -> Result<Target, Error> {
        Ok(Target {
            name: self.dotted_name(expected)?,
            indirection: Vec::new(),
        })
    }

    /// Reads the name [`Self::peek_qualified`] finds.
    fn dotted_name(&mut self, expected: &'static str) -> Result<ObjectName, Error> {
        let Some((name, length)) = self.peek_qualified() else {
            return Err(self.syntax(expected));
        };
        self.index += length;
        Ok(name)
    }

    /// Reads `EXIT` or `CONTINUE`, refusing one that reaches no loop where
    /// PostgreSQL requires one.
    fn exit(&mut self, kind: ExitKind) -> Result<Exit, Error> {
        self.index += 1;
        let label = if matches!(self.peek(), Some(Token::Word(_))) && !self.at("WHEN") {
            Some(self.name("a label")?)
        } else {
            None
        };
        let target = self.enclosing.iter().rev().find(|enclosing| match &label {
            Some(label) => enclosing.label.as_ref() == Some(label),
            None => enclosing.looped,
        });
        let refused = match kind {
            ExitKind::Exit => label.is_none() && target.is_none(),
            ExitKind::Continue => !target.is_some_and(|enclosing| enclosing.looped),
        };
        if refused {
            return Err(Error::NoEnclosingLoop { kind, label });
        }
        let condition = if self.eat("WHEN") {
            Some(self.expression_until_semicolon()?)
        } else {
            None
        };
        Ok(Exit {
            kind,
            label,
            condition,
        })
    }

    /// Reads `RETURN`, `RETURN NEXT` or `RETURN QUERY`. A `RETURN NEXT`
    /// without an expression takes no step.
    fn return_statement(&mut self) -> Result<Option<Node>, Error> {
        self.index += 1;
        if self.eat("QUERY") {
            return Ok(Some(if self.eat("EXECUTE") {
                Node::Dynamic {
                    dynamic: self.dynamic(End::Semicolon, false)?.0,
                    into: None,
                }
            } else {
                Node::Statement {
                    statement: self.statement_until_semicolon()?,
                    into: None,
                }
            }));
        }
        let next = self.eat("NEXT");
        let value = if self.peek() == Some(&Token::SemiColon) {
            None
        } else {
            Some(self.expression_until_semicolon()?)
        };
        Ok(if next {
            value.map(evaluated)
        } else {
            Some(Node::Return { value })
        })
    }

    /// Reads the condition of a handler. A name PostgreSQL folds to
    /// `sqlstate`, quoted or not, introduces an SQLSTATE code.
    fn handler_condition(&mut self) -> Result<Condition, Error> {
        let Some(Token::Word(word)) = self.peek() else {
            return Err(self.syntax("an exception condition"));
        };
        if self.at_any(&["OR", "THEN"]) {
            return Err(self.syntax("an exception condition"));
        }
        let name = identifier(word);
        self.index += 1;
        Ok(if name == "sqlstate" {
            Condition::Sqlstate(self.sqlstate_code()?)
        } else {
            Condition::Name(name)
        })
    }

    /// Reads the string constant after `SQLSTATE`, which PostgreSQL accepts
    /// only as five digits or upper-case ASCII letters.
    fn sqlstate_code(&mut self) -> Result<String, Error> {
        let Some(code) = self
            .peek()
            .and_then(string_literal)
            .filter(|code| is_sqlstate(code))
            .map(String::from)
        else {
            return Err(self.syntax("an SQLSTATE code"));
        };
        self.index += 1;
        Ok(code)
    }

    fn raise(&mut self) -> Result<Raise, Error> {
        const LEVELS: [(&str, RaiseLevel); 6] = [
            ("DEBUG", RaiseLevel::Debug),
            ("LOG", RaiseLevel::Log),
            ("INFO", RaiseLevel::Info),
            ("NOTICE", RaiseLevel::Notice),
            ("WARNING", RaiseLevel::Warning),
            ("EXCEPTION", RaiseLevel::Exception),
        ];
        self.index += 1;
        let mut raise = Raise {
            level: RaiseLevel::Exception,
            condition: None,
            format: None,
            arguments: Vec::new(),
            options: Vec::new(),
        };
        if self.peek() == Some(&Token::SemiColon) {
            return Ok(raise);
        }
        if let Some(level) = self.peek().and_then(|token| keyword_in(&LEVELS, token)) {
            raise.level = level;
            self.index += 1;
        }
        if let Some(format) = self.peek().and_then(string_literal).map(String::from) {
            raise.format = Some(format);
            self.index += 1;
            while self.eat_token(&Token::Comma) {
                raise.arguments.push(*self.expression("`;`", |token, _| {
                    matches!(token, Token::Comma | Token::SemiColon) || is_keyword(token, "USING")
                })?);
            }
        } else if self.eat("SQLSTATE") {
            raise.condition = Some(Condition::Sqlstate(self.sqlstate_code()?));
        } else if !self.at("USING")
            && let Some(Token::Word(word)) = self.peek()
        {
            raise.condition = Some(Condition::Name(identifier(word)));
            self.index += 1;
        }
        if self.eat("USING") {
            loop {
                let Some(kind) = self
                    .peek()
                    .and_then(|token| keyword_in(&RaiseOptionKind::KEYWORDS, token))
                else {
                    return Err(self.syntax("a RAISE option"));
                };
                self.index += 1;
                if !(self.eat_token(&Token::Eq) || self.eat_token(&Token::Assignment)) {
                    return Err(self.syntax("`=`"));
                }
                let value = *self.expression("`;`", |token, _| {
                    matches!(token, Token::Comma | Token::SemiColon)
                })?;
                raise.options.push(RaiseOption { kind, value });
                if !self.eat_token(&Token::Comma) {
                    break;
                }
            }
        }
        Ok(raise)
    }

    fn assert(&mut self) -> Result<Assert, Error> {
        self.index += 1;
        let condition = self.expression("`;`", |token, _| {
            matches!(token, Token::Comma | Token::SemiColon)
        })?;
        let message = if self.eat_token(&Token::Comma) {
            Some(self.expression_until_semicolon()?)
        } else {
            None
        };
        Ok(Assert { condition, message })
    }

    fn open(&mut self) -> Result<Open, Error> {
        self.index += 1;
        let cursor = self.dotted_name("a cursor name")?;
        if self.eat("NO") {
            self.expect("SCROLL")?;
        } else {
            self.eat("SCROLL");
        }
        if self.eat("FOR") {
            return Ok(if self.eat("EXECUTE") {
                Open::Dynamic {
                    cursor,
                    dynamic: self.dynamic(End::Semicolon, false)?.0,
                }
            } else {
                Open::Query {
                    cursor,
                    query: self.statement_until_semicolon()?,
                }
            });
        }
        let Some(query) = self.bound_cursor(&cursor).cloned().map(Box::new) else {
            return Err(Error::UnboundCursor {
                name: stored_parts(&cursor).join("."),
            });
        };
        let arguments = if self.eat_token(&Token::LParen) {
            self.cursor_arguments()?
        } else {
            Vec::new()
        };
        Ok(Open::Bound(BoundCursor {
            name: cursor,
            arguments,
            query,
        }))
    }

    /// Reads `FETCH` or `MOVE`, whose direction may count rows with an
    /// expression, and the targets `FETCH` assigns.
    fn fetch(&mut self, nodes: &mut Vec<Node>) -> Result<(), Error> {
        const DIRECTIONS: [&str; 9] = [
            "NEXT", "PRIOR", "FIRST", "LAST", "ABSOLUTE", "RELATIVE", "FORWARD", "BACKWARD", "ALL",
        ];
        let fetch = self.at("FETCH");
        self.index += 1;
        let direction = self.segment("`;`", |token, _| {
            *token == Token::SemiColon
                || is_keyword(token, "FROM")
                || is_keyword(token, "IN")
                || is_keyword(token, "INTO")
        })?;
        if self.eat("FROM") || self.eat("IN") {
            let count = &self.tokens[direction];
            let skipped = count
                .iter()
                .take_while(|token| DIRECTIONS.iter().any(|word| is_keyword(token, word)))
                .count();
            if skipped < count.len() {
                nodes.push(evaluated(select(&count[skipped..])?));
            }
        } else {
            // Without a direction, the cursor comes first.
            self.index = direction.start;
        }
        let cursor = self.dotted_name("a cursor")?;
        if fetch {
            self.expect("INTO")?;
            let targets = self.targets("an INTO target")?;
            nodes.push(Node::Fetch(Fetch { cursor, targets }));
        }
        Ok(())
    }

    /// Reads `GET [CURRENT | STACKED] DIAGNOSTICS target = item, ...`.
    fn diagnostics(&mut self) -> Result<Diagnostics, Error> {
        self.index += 1;
        let stacked = self.eat("STACKED");
        if !stacked {
            self.eat("CURRENT");
        }
        self.expect("DIAGNOSTICS")?;
        let mut items = Vec::new();
        loop {
            let target = self.plain_target("a diagnostics target")?;
            if !(self.eat_token(&Token::Eq) || self.eat_token(&Token::Assignment)) {
                return Err(self.syntax("`=`"));
            }
            let Some(kind) = self
                .peek()
                .and_then(|token| keyword_in(&DiagnosticsKind::KEYWORDS, token))
                .filter(|kind| kind.allowed(stacked))
            else {
                return Err(self.syntax(if stacked {
                    "an item of GET STACKED DIAGNOSTICS"
                } else {
                    "an item of GET CURRENT DIAGNOSTICS"
                }));
            };
            self.index += 1;
            items.push(DiagnosticsItem { target, kind });
            if !self.eat_token(&Token::Comma) {
                return Ok(Diagnostics { stacked, items });
            }
        }
    }

    /// True when the statement starts with an assignment target, a name or
    /// `$n` with any fields and subscripts, followed by `:=` or `=`.
    fn at_assignment(&self) -> bool {
        if !matches!(self.peek(), Some(Token::Word(_) | Token::Placeholder(_))) {
            return false;
        }
        let mut at = self.index + 1;
        loop {
            match self.tokens.get(at) {
                Some(Token::Period) => at += 2,
                Some(Token::LBracket) => {
                    let mut depth = 0_usize;
                    loop {
                        match self.tokens.get(at) {
                            Some(Token::LBracket) => depth += 1,
                            Some(Token::RBracket) => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            None | Some(Token::SemiColon) => return false,
                            _ => {}
                        }
                        at += 1;
                    }
                    at += 1;
                }
                Some(Token::Assignment | Token::Eq) => return true,
                _ => return false,
            }
        }
    }

    /// Reads an assignment, which [`Self::at_assignment`] found.
    fn assignment(&mut self) -> Result<Assignment, Error> {
        let name = self.dotted_name("a variable")?;
        let mut indirection = Vec::new();
        loop {
            if self.eat_token(&Token::LBracket) {
                indirection.push(self.subscript()?);
            } else if self.eat_token(&Token::Period) {
                let Some(Token::Word(field)) = self.peek() else {
                    return Err(self.syntax("a field"));
                };
                indirection.push(Indirection::Field(field.to_ident(Span::empty())));
                self.index += 1;
            } else {
                break;
            }
        }
        // Past the `:=` or `=`.
        self.index += 1;
        Ok(Assignment {
            target: Target { name, indirection },
            value: self.expression_until_semicolon()?,
        })
    }

    /// Reads a subscript after its `[`, through the `]`.
    fn subscript(&mut self) -> Result<Indirection, Error> {
        let lower = self.subscript_bound()?;
        let subscript = if self.eat_token(&Token::Colon) {
            Indirection::Slice {
                lower,
                upper: self.subscript_bound()?,
            }
        } else {
            Indirection::Index(lower.ok_or_else(|| self.syntax("a subscript"))?)
        };
        self.expect_token(&Token::RBracket, "`]`")?;
        Ok(subscript)
    }

    /// Reads the expression of a subscript up to its `:` or `]`, if any.
    fn subscript_bound(&mut self) -> Result<Option<Box<Query>>, Error> {
        if matches!(self.peek(), Some(Token::Colon | Token::RBracket)) {
            return Ok(None);
        }
        self.expression("`]`", |token, _| {
            matches!(token, Token::Colon | Token::RBracket)
        })
        .map(Some)
    }

    /// Reads an SQL statement up to its `;`, taking out its `INTO` clause
    /// the way PostgreSQL does.
    ///
    /// An `INTO` right after `INSERT` or `MERGE`, or anywhere in `IMPORT`,
    /// belongs to the SQL. A routine defined with `BEGIN ATOMIC` keeps the
    /// `;` inside its body, which PostgreSQL finds by counting `BEGIN` and
    /// `CASE` against `END`.
    fn sql_statement(&mut self) -> Result<(Box<Statement>, Option<IntoTargets>), Error> {
        let start = self.index;
        let starts = |offset: usize, keyword: &str| {
            self.tokens
                .get(start + offset)
                .is_some_and(|token| is_keyword(token, keyword))
        };
        let defines_routine = starts(0, "CREATE") && {
            let name = if starts(1, "OR") && starts(2, "REPLACE") {
                3
            } else {
                1
            };
            starts(name, "FUNCTION") || starts(name, "PROCEDURE")
        };
        let imports = starts(0, "IMPORT");
        let mut parens = 0_usize;
        let mut begins = 0_usize;
        let mut into: Option<(Range<usize>, IntoTargets)> = None;

        loop {
            let Some(token) = self.peek() else {
                return Err(self.syntax("`;`"));
            };
            match token {
                Token::SemiColon if parens == 0 && begins == 0 => break,
                Token::LParen => parens += 1,
                Token::RParen => parens = parens.saturating_sub(1),
                _ => {}
            }
            if defines_routine && parens == 0 {
                if is_keyword(token, "BEGIN") || is_keyword(token, "CASE") {
                    begins += 1;
                } else if is_keyword(token, "END") && begins > 0 {
                    begins -= 1;
                }
            }
            let into_target = is_keyword(token, "INTO")
                && !imports
                && !self.index.checked_sub(1).is_some_and(|before| {
                    is_keyword(&self.tokens[before], "INSERT")
                        || is_keyword(&self.tokens[before], "MERGE")
                });
            if into_target {
                if into.is_some() {
                    return Err(self.syntax("a single INTO"));
                }
                let clause = self.index;
                self.index += 1;
                let targets = self.read_into()?;
                into = Some((clause..self.index, targets));
            } else {
                self.index += 1;
            }
        }

        let Some((range, targets)) = into else {
            return Ok((parse(&self.tokens[start..self.index])?, None));
        };
        let mut kept = Vec::with_capacity(self.index - start - range.len());
        kept.extend_from_slice(&self.tokens[start..range.start]);
        kept.extend_from_slice(&self.tokens[range.end..self.index]);
        Ok((parse(&kept)?, Some(targets)))
    }
}

/// Parses `tokens` as one SQL statement.
fn parse(tokens: &[Token]) -> Result<Box<Statement>, Error> {
    parse_all(tokens, false, Parser::parse_statement).map(Box::new)
}

/// Parses `tokens`, an expression, as the `SELECT` that evaluates it.
fn select(tokens: &[Token]) -> Result<Box<Query>, Error> {
    parse_all(tokens, true, Parser::parse_query)
}

/// The step that evaluates `select`, an expression read as its `SELECT`.
fn evaluated(select: Box<Query>) -> Node {
    Node::Statement {
        statement: Box::new(Statement::Query(select)),
        into: None,
    }
}

/// Reads all of `tokens` with `read`, behind a `SELECT` when `select` holds.
fn parse_all<T>(
    tokens: &[Token],
    select: bool,
    read: impl FnOnce(&mut Parser<'static>) -> Result<T, ParserError>,
) -> Result<T, Error> {
    let mut owned = Vec::with_capacity(tokens.len() + 1);
    if select {
        owned.push(Token::Word(Word {
            value: "SELECT".into(),
            quote_style: None,
            keyword: Keyword::SELECT,
        }));
    }
    owned.extend_from_slice(tokens);
    let mut parser = Parser::new(&PostgreSqlDialect {}).with_tokens(owned);
    read(&mut parser)
        .and_then(|read| parser.expect_token(&Token::EOF).map(|_| read))
        .map_err(|source| Error::EmbeddedSql {
            sql: render(tokens, select),
            source,
        })
}

/// Reads the type a declaration writes: `name%TYPE` or `name%ROWTYPE`, each
/// with any `[]` after it, or else a type named directly.
fn declared_type(tokens: &[Token]) -> Result<DeclaredType, Error> {
    let mut end = tokens.len();
    loop {
        match &tokens[..end] {
            [.., Token::LBracket, Token::RBracket] => end -= 2,
            [.., Token::LBracket, Token::Number(..), Token::RBracket] => end -= 3,
            _ => break,
        }
    }
    let array = end < tokens.len();
    if let [of @ .., Token::Mod, reference] = &tokens[..end]
        && !of.is_empty()
    {
        let of = || parse_all(of, false, |parser| parser.parse_object_name(false));
        if is_keyword(reference, "TYPE") {
            return Ok(DeclaredType::ColumnType { of: of()?, array });
        }
        if is_keyword(reference, "ROWTYPE") {
            return Ok(DeclaredType::RowType { of: of()?, array });
        }
    }
    parse_all(tokens, false, Parser::parse_data_type).map(DeclaredType::Data)
}

/// The text of `tokens`, one space apart, for an error message.
fn render(tokens: &[Token], select: bool) -> String {
    let mut text = String::from(if select { "SELECT" } else { "" });
    for token in tokens {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&token.to_string());
    }
    text
}

/// True for `keyword` written unquoted, in any case.
fn is_keyword(token: &Token, keyword: &str) -> bool {
    matches!(token, Token::Word(word) if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(keyword))
}

/// True for the `..` of an integer range, which [`spaced_ranges`] split into
/// two periods.
fn is_range_dots(token: &Token, next: Option<&Token>) -> bool {
    *token == Token::Period && next == Some(&Token::Period)
}

/// The value `keywords` pairs with `token`, a keyword written unquoted.
fn keyword_in<T: Copy>(keywords: &[(&str, T)], token: &Token) -> Option<T> {
    keywords
        .iter()
        .find(|(keyword, _)| is_keyword(token, keyword))
        .map(|&(_, value)| value)
}

/// The text of a string constant.
fn string_literal(token: &Token) -> Option<&str> {
    match token {
        Token::SingleQuotedString(text)
        | Token::EscapedStringLiteral(text)
        | Token::UnicodeStringLiteral(text)
        | Token::NationalStringLiteral(text) => Some(text),
        Token::DollarQuotedString(quoted) => Some(&quoted.value),
        _ => None,
    }
}

/// Whether `code` is an SQLSTATE code as PostgreSQL accepts one, five digits
/// or upper-case ASCII letters.
fn is_sqlstate(code: &str) -> bool {
    code.len() == 5
        && code
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
}

/// The name an identifier denotes, as PostgreSQL folds it.
fn identifier(word: &Word) -> String {
    folded(&word.value, word.quote_style.is_some())
}

/// The position a parameter's `$n` names.
fn parameter_number(placeholder: &str) -> Option<usize> {
    placeholder
        .strip_prefix('$')
        .and_then(|digits| digits.parse().ok())
}

/// The one variable a range or cursor `FOR` loop declares, from the
/// targets written before its `IN`.
fn loop_variable(targets: &[Target]) -> Result<String, Error> {
    if let [Target { name, indirection }] = targets
        && indirection.is_empty()
        && let [part] = name.0.as_slice()
        && let Some(ident) = part.as_ident()
    {
        return Ok(stored(ident));
    }
    let written: Vec<String> = targets
        .iter()
        .map(|target| target.name.to_string())
        .collect();
    Err(Error::Syntax {
        expected: "one loop variable",
        found: written.join(", "),
    })
}

/// The name an identifier written as `value` denotes, as PostgreSQL folds
/// it. A `quoted` one stays as written, and any other is ASCII-folded to
/// lower case.
pub(crate) fn folded(value: &str, quoted: bool) -> String {
    if quoted {
        value.into()
    } else {
        value.to_ascii_lowercase()
    }
}
