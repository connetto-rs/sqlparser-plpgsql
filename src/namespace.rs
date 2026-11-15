//! Which variable a name written in a PL/pgSQL body reaches, as PostgreSQL's
//! `plpgsql_ns_lookup` answers it.
//!
//! PL/pgSQL nests levels of names. The outermost belongs to the function,
//! labelled with the function's name, and holds its parameters, the variables
//! a trigger function receives and `FOUND`. Inside it each block opens a
//! level for its declarations, labelled with the block's label, and each loop
//! opens one for the variable a `FOR` loop declares. A cursor declared with
//! its query opens one, labelled with the cursor's name, for the parameters
//! only that query reads. An `EXCEPTION` section adds `SQLSTATE` and
//! `SQLERRM` to its block's level.
//!
//! PostgreSQL compiles each statement and expression with the names in reach
//! where it reads that SQL, which [`Block::sites`] answers for every one.

use alloc::{
    borrow::Cow,
    boxed::Box,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    fmt,
    hash::{Hash, Hasher},
};

use sqlparser::ast::{Ident, ObjectName, Query, Statement};

use crate::embedded::{
    AliasTarget, Block, BoundCursor, Branch, Cursor, CursorParameter, Declaration, DeclarationKind,
    DeclaredType, Dynamic, Indirection, Loop, LoopControl, Node, Open, Target,
};

/// An error refusing a body PostgreSQL does not compile, found while
/// following the names it declares.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `ALIAS FOR` names no variable in reach where the alias is declared.
    #[error("alias '{alias}' is for '{target}', which names no variable in reach")]
    UnknownAliasTarget {
        /// The alias, folded as PostgreSQL folds an identifier.
        alias: String,
        /// The name the alias is for, as written.
        target: String,
    },
    /// `OPEN` or a cursor `FOR` loop names no cursor declared with a query,
    /// once `holds_row` decides which variable each name reaches.
    #[error("'{name}' names no cursor declared with a query")]
    UnboundCursor {
        /// The cursor name, each part folded as PostgreSQL folds it and
        /// joined by periods.
        name: String,
    },
}

/// What PL/pgSQL learns about a function from its definition rather than
/// from its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// The function's name, as PostgreSQL stores it, which labels the level
    /// holding the parameters.
    pub name: String,
    /// The name of each parameter as PostgreSQL stores it, or `None` for an
    /// unnamed one, in the order declared and counting every mode.
    ///
    /// The columns of `RETURNS TABLE` follow as output parameters.
    pub parameters: Vec<Option<String>>,
    /// What else the function's level holds.
    pub kind: FunctionKind,
}

/// What the function's level holds besides the parameters and `FOUND`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    /// Nothing else.
    Plain,
    /// `$0`, which PL/pgSQL declares when the result type is polymorphic and
    /// no parameter is an output one.
    PolymorphicResult,
    /// The variables of a trigger function, `NEW`, `OLD` and the `TG_` ones.
    Trigger,
    /// The variables of an event trigger function, `TG_EVENT` and `TG_TAG`.
    EventTrigger,
}

/// A variable PostgreSQL gives a trigger or event trigger function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TriggerVariable {
    /// `NEW`, the row after the change.
    New,
    /// `OLD`, the row before the change.
    Old,
    /// `TG_NAME`.
    TgName,
    /// `TG_WHEN`.
    TgWhen,
    /// `TG_LEVEL`.
    TgLevel,
    /// `TG_OP`.
    TgOp,
    /// `TG_RELID`.
    TgRelid,
    /// `TG_RELNAME`.
    TgRelname,
    /// `TG_TABLE_NAME`.
    TgTableName,
    /// `TG_TABLE_SCHEMA`.
    TgTableSchema,
    /// `TG_NARGS`.
    TgNargs,
    /// `TG_ARGV`.
    TgArgv,
    /// `TG_EVENT`, of an event trigger function.
    TgEvent,
    /// `TG_TAG`, of an event trigger function.
    TgTag,
}

impl TriggerVariable {
    /// The variables of a trigger function, in the order PostgreSQL declares
    /// them.
    const TRIGGER: [Self; 12] = [
        Self::New,
        Self::Old,
        Self::TgName,
        Self::TgWhen,
        Self::TgLevel,
        Self::TgOp,
        Self::TgRelid,
        Self::TgRelname,
        Self::TgTableName,
        Self::TgTableSchema,
        Self::TgNargs,
        Self::TgArgv,
    ];

    /// The variables of an event trigger function.
    const EVENT_TRIGGER: [Self; 2] = [Self::TgEvent, Self::TgTag];

    /// The name the variable is reached by, as PostgreSQL stores it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Old => "old",
            Self::TgName => "tg_name",
            Self::TgWhen => "tg_when",
            Self::TgLevel => "tg_level",
            Self::TgOp => "tg_op",
            Self::TgRelid => "tg_relid",
            Self::TgRelname => "tg_relname",
            Self::TgTableName => "tg_table_name",
            Self::TgTableSchema => "tg_table_schema",
            Self::TgNargs => "tg_nargs",
            Self::TgArgv => "tg_argv",
            Self::TgEvent => "tg_event",
            Self::TgTag => "tg_tag",
        }
    }
}

/// A variable a name in the body reaches.
///
/// Two names compare equal when they reach the same variable of the same
/// body, whatever they are written as.
#[derive(Clone, Debug)]
pub struct Name {
    id: usize,
    kind: NameKind,
}

impl Name {
    /// The variable's position among the variables of the body in the order
    /// PostgreSQL declares them, which is the parameters, then the rest of
    /// the function's level, then the body's in the order written.
    ///
    /// `$n` shares the id of parameter `n`, and an alias that of the variable
    /// it is for.
    #[must_use]
    pub const fn id(&self) -> usize {
        self.id
    }

    /// What declares the variable.
    #[must_use]
    pub const fn kind(&self) -> &NameKind {
        &self.kind
    }

    /// Whether the variable holds a row, when its kind alone settles it.
    fn fixed_row(&self) -> Option<bool> {
        match &self.kind {
            NameKind::Parameter { .. } | NameKind::CursorParameter { .. } => None,
            NameKind::Declared { declaration, .. }
                if matches!(declaration.kind, DeclarationKind::Variable(_)) =>
            {
                None
            }
            NameKind::LoopRecord { .. }
            | NameKind::Trigger(TriggerVariable::New | TriggerVariable::Old) => Some(true),
            NameKind::Declared { .. }
            | NameKind::ReturnValue
            | NameKind::Trigger(_)
            | NameKind::Found
            | NameKind::LoopIndex { .. }
            | NameKind::SqlState
            | NameKind::SqlErrm => Some(false),
        }
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Name {}

impl Hash for Name {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// What declares a variable.
#[derive(Clone, Debug, PartialEq)]
pub enum NameKind {
    /// The function's parameter `$position`, counting from 1.
    Parameter {
        /// The position, counting from 1.
        position: usize,
    },
    /// `$0`, which holds the result of a function whose result type is
    /// polymorphic.
    ReturnValue,
    /// A variable PostgreSQL gives a trigger or event trigger function.
    Trigger(TriggerVariable),
    /// `FOUND`.
    Found,
    /// A variable or cursor a `DECLARE` section binds.
    ///
    /// An alias never appears here, since it reaches the variable it is for.
    Declared {
        /// The declaration.
        declaration: Arc<Declaration>,
        /// The variable whose type a `name%TYPE` copies, where the
        /// declaration stands.
        ///
        /// `None` when the declaration writes no `%TYPE` or its name reaches
        /// no variable there, which leaves it to name a column.
        type_of: Option<Box<Name>>,
    },
    /// A parameter of a cursor declared with its query, which only that
    /// query reads.
    CursorParameter {
        /// The parameter.
        parameter: Arc<CursorParameter>,
        /// The variable whose type a `name%TYPE` copies, where the parameter
        /// is declared.
        ///
        /// `None` when the parameter writes no `%TYPE` or its name reaches no
        /// variable there, which leaves it to name a column.
        type_of: Option<Box<Name>>,
    },
    /// The integer a `FOR i IN lower .. upper` loop declares for its
    /// statements.
    LoopIndex {
        /// The name the loop declares the integer under, as PostgreSQL
        /// stores it.
        variable: String,
    },
    /// The record a `FOR r IN cursor` loop declares for its statements.
    LoopRecord {
        /// The name the loop declares the record under, as PostgreSQL
        /// stores it.
        variable: String,
        /// The query the cursor's declaration binds, whose rows the record
        /// holds in turn.
        query: Arc<Statement>,
    },
    /// `SQLSTATE`, which an `EXCEPTION` section declares for its handlers.
    SqlState,
    /// `SQLERRM`, which an `EXCEPTION` section declares for its handlers.
    SqlErrm,
}

/// The names in reach at one point of a body.
///
/// It owns what it holds, so it outlives the [`Block`] it was read from.
#[derive(Clone, Debug)]
pub struct Namespace {
    level: Names,
}

impl Namespace {
    /// The variable the name written as `parts` reaches, with how many of
    /// the parts name it. Each part is an identifier as PostgreSQL stores
    /// it, or `$n` for a parameter.
    ///
    /// From the innermost level outwards, a level answers a variable it
    /// declares under the first part, then one it declares under the second
    /// part when its label is the first. A variable is passed over when
    /// another part follows and it holds no row, since only a row has fields
    /// to read. `None` when nothing answers, and for no parts or more than
    /// three, which PostgreSQL never reads as a variable.
    ///
    /// `holds_row` answers for a parameter, a declared variable or a cursor
    /// parameter, whose type decides. Every other variable holds a row only
    /// when it is a loop's record, `NEW` or `OLD`.
    pub fn lookup<S: AsRef<str>>(
        &self,
        parts: &[S],
        holds_row: impl Fn(&Name) -> bool,
    ) -> Option<(&Name, usize)> {
        self.level.lookup(parts, &|name| row(name, &holds_row))
    }
}

/// Whether `name` holds a row, asking `holds_row` when its kind does not
/// settle it.
fn row(name: &Name, holds_row: &dyn Fn(&Name) -> bool) -> bool {
    name.fixed_row().unwrap_or_else(|| holds_row(name))
}

/// SQL a body runs, as [`embedded_sql`](crate::embedded_sql) reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sql<'body> {
    /// A statement or query, such as a [`Node::Statement`], the query of a
    /// `FOR` loop over rows or the query of a cursor that opens.
    Statement(&'body Statement),
    /// The `SELECT` that evaluates an expression, such as a condition, a
    /// loop bound, a cursor argument, a default or an assigned value.
    Expression(&'body Query),
    /// SQL built at run time, whose `USING` arguments read the same names
    /// as its command.
    Dynamic(&'body Dynamic),
}

/// SQL a body runs, with the names in reach where PostgreSQL compiles it.
#[derive(Clone, Debug)]
pub struct Site<'body> {
    /// The SQL.
    pub sql: Sql<'body>,
    /// The names the SQL reaches.
    pub namespace: Namespace,
}

/// A place a body assigns variables, with the names in reach there.
#[derive(Clone, Debug)]
pub struct WriteSite<'body> {
    /// What assigns.
    pub write: Write<'body>,
    /// The names the targets of the write reach.
    ///
    /// For a declaration, that is the names declared before it. For a loop,
    /// that is the loop's label alone inside the enclosing names, and the
    /// variable a range or cursor `FOR` loop declares is [`Self::declared`].
    pub namespace: Namespace,
    /// The variable the write declares and assigns, for a declaration and
    /// for a range or cursor `FOR` loop.
    pub declared: Option<Name>,
}

/// What a [`WriteSite`] assigns with, borrowed from the [`Block`].
#[derive(Clone, Copy, Debug)]
pub enum Write<'body> {
    /// A variable or cursor declaration, which on entering its block
    /// assigns the default, a null, or for a cursor its own name.
    Declaration(&'body Declaration),
    /// A step assigning its targets. That is an assignment, a statement or
    /// `EXECUTE` with `INTO`, `FETCH`, `GET DIAGNOSTICS`, `OPEN`, which
    /// names a cursor holding null, or a `FOR` or `FOREACH` loop. It is also
    /// a `CALL`, which assigns the arguments its procedure's `INOUT` and
    /// `OUT` parameters take, as only the procedure's signature tells.
    Node(&'body Node),
}

impl Block {
    /// Every statement and expression of the body this block reads, in body
    /// order, with the names PostgreSQL compiles each with.
    ///
    /// A block yields each declaration's default, which reads the names
    /// declared before it, then its statements, then each handler's. A
    /// branch yields its condition before its statements, and a `CASE` its
    /// subject before every branch. A loop yields its control, which reads
    /// the names outside the variable the loop declares, before its
    /// statements. An assignment yields its target's subscripts, then its
    /// value. `RAISE` yields its arguments, then its options, and `ASSERT`
    /// its condition, then its message. A cursor that opens yields its
    /// arguments, then its query, which reads the names in reach at the
    /// cursor's declaration and the cursor's parameters.
    ///
    /// `holds_row` answers whether a parameter, a declared variable or a
    /// cursor parameter holds a row, as for [`Namespace::lookup`], which an
    /// alias, a `%TYPE` or a cursor naming a field needs.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownAliasTarget`] when `ALIAS FOR` names no
    /// variable in reach, and [`Error::UnboundCursor`] when `OPEN` or a
    /// cursor `FOR` loop names no cursor declared with a query.
    ///
    /// # Example
    ///
    /// ```
    /// use sqlparser_plpgsql::{
    ///     embedded_sql,
    ///     namespace::{FunctionKind, NameKind, Signature, Sql},
    /// };
    ///
    /// let block = embedded_sql("DECLARE p integer; BEGIN RETURN f.p + p; END")?;
    /// let signature = Signature {
    ///     name: "f".into(),
    ///     parameters: vec![Some("p".into())],
    ///     kind: FunctionKind::Plain,
    /// };
    /// let sites = block
    ///     .sites(&signature, |_| false)
    ///     .expect("the body declares no alias and opens no cursor");
    /// let Sql::Expression(returned) = sites[0].sql else { unreachable!() };
    /// assert_eq!(returned.to_string(), "SELECT f.p + p");
    /// let (name, consumed) = sites[0]
    ///     .namespace
    ///     .lookup(&["f", "p"], |_| false)
    ///     .expect("f.p reaches the parameter");
    /// assert_eq!(name.kind(), &NameKind::Parameter { position: 1 });
    /// assert_eq!(consumed, 2);
    /// # Ok::<(), sqlparser_plpgsql::embedded::Error>(())
    /// ```
    pub fn sites<'body>(
        &'body self,
        signature: &Signature,
        holds_row: impl Fn(&Name) -> bool,
    ) -> Result<Vec<Site<'body>>, Error> {
        Ok(self.walk(signature, &holds_row)?.sites)
    }

    /// Every place the body this block reads assigns variables, in body
    /// order, with the names in reach at each.
    ///
    /// Each variable a name reaches has one [`Name::id`] in a reading, so the
    /// targets of two writes name the same variable when their lookups give
    /// equal names. The values a write assigns and any subscripts of its
    /// targets are among [`Self::sites`]. `holds_row` answers as for
    /// [`Self::sites`].
    ///
    /// # Errors
    ///
    /// Returns the errors [`Self::sites`] returns.
    ///
    /// # Example
    ///
    /// ```
    /// use sqlparser_plpgsql::{
    ///     embedded::Node,
    ///     embedded_sql,
    ///     namespace::{FunctionKind, Signature, Write},
    /// };
    ///
    /// let block = embedded_sql("DECLARE s text; BEGIN s := 'SELECT ' || p; EXECUTE s; END")?;
    /// let signature = Signature {
    ///     name: "f".into(),
    ///     parameters: vec![Some("p".into())],
    ///     kind: FunctionKind::Plain,
    /// };
    /// let writes = block
    ///     .write_sites(&signature, |_| false)
    ///     .expect("the body declares no alias and opens no cursor");
    /// let declared = writes[0].declared.as_ref().expect("s is declared");
    /// let Write::Node(Node::Assignment(assignment)) = writes[1].write else {
    ///     unreachable!()
    /// };
    /// assert_eq!(assignment.value.to_string(), "SELECT 'SELECT ' || p");
    /// let (assigned, _) = writes[1]
    ///     .namespace
    ///     .lookup(&["s"], |_| false)
    ///     .expect("s is in reach");
    /// assert_eq!(assigned, declared);
    /// # Ok::<(), sqlparser_plpgsql::embedded::Error>(())
    /// ```
    pub fn write_sites<'body>(
        &'body self,
        signature: &Signature,
        holds_row: impl Fn(&Name) -> bool,
    ) -> Result<Vec<WriteSite<'body>>, Error> {
        Ok(self.walk(signature, &holds_row)?.write_sites)
    }

    /// Walks the body, numbering its variables and recording every site.
    fn walk<'body, 'predicate>(
        &'body self,
        signature: &Signature,
        holds_row: &'predicate dyn Fn(&Name) -> bool,
    ) -> Result<Collector<'body, 'predicate>, Error> {
        let mut collector = Collector {
            holds_row,
            next_id: 0,
            cursors: BTreeMap::new(),
            sites: Vec::new(),
            write_sites: Vec::new(),
        };
        let function = collector.function_level(signature);
        collector.block(self, &function)?;
        Ok(collector)
    }
}

/// Walks a body in order, numbering its variables and recording each
/// statement and expression with the names in reach where PostgreSQL reads
/// it, and each write with the names its targets reach.
struct Collector<'body, 'predicate> {
    holds_row: &'predicate dyn Fn(&Name) -> bool,
    next_id: usize,
    /// The names the query of each cursor declared with one reads, by the id
    /// of the cursor.
    cursors: BTreeMap<usize, Names>,
    sites: Vec<Site<'body>>,
    write_sites: Vec<WriteSite<'body>>,
}

impl<'body> Collector<'body, '_> {
    /// A variable of the next id.
    fn name(&mut self, kind: NameKind) -> Name {
        let id = self.next_id;
        self.next_id += 1;
        Name { id, kind }
    }

    /// The level every body starts in, in the order PostgreSQL declares it.
    fn function_level(&mut self, signature: &Signature) -> Names {
        let mut level = Level::new(Some(Cow::Owned(signature.name.clone())), None);
        for (index, name) in signature.parameters.iter().enumerate() {
            let position = index + 1;
            let parameter = self.name(NameKind::Parameter { position });
            if let Some(name) = name {
                level.declare(name.clone(), parameter.clone());
            }
            level.declare(format!("${position}"), parameter);
        }
        let implicit: &[TriggerVariable] = match signature.kind {
            FunctionKind::Plain => &[],
            FunctionKind::PolymorphicResult => {
                let result = self.name(NameKind::ReturnValue);
                level.declare("$0", result);
                &[]
            }
            FunctionKind::Trigger => &TriggerVariable::TRIGGER,
            FunctionKind::EventTrigger => &TriggerVariable::EVENT_TRIGGER,
        };
        for &variable in implicit {
            let name = self.name(NameKind::Trigger(variable));
            level.declare(variable.name(), name);
        }
        let found = self.name(NameKind::Found);
        level.declare("found", found);
        level
    }

    fn block(&mut self, block: &'body Block, outer: &Names) -> Result<(), Error> {
        let mut level = Level::new(block.label.clone().map(Cow::Owned), Some(outer));
        for declaration in &block.declarations {
            let name = match &declaration.kind {
                DeclarationKind::Alias(target) => {
                    let name = self.aliased(&level, declaration, target)?;
                    level.declare(declaration.name.clone(), name);
                    continue;
                }
                DeclarationKind::Variable(variable) => {
                    if let Some(default) = &variable.default {
                        self.site(Sql::Expression(default), &level);
                    }
                    let type_of = self.type_of(&level, &variable.data_type);
                    self.name(NameKind::Declared {
                        declaration: Arc::new(declaration.clone()),
                        type_of,
                    })
                }
                DeclarationKind::Cursor(cursor) => {
                    let query = self.cursor_parameters(&level, declaration, cursor);
                    let name = self.name(NameKind::Declared {
                        declaration: Arc::new(declaration.clone()),
                        type_of: None,
                    });
                    self.cursors.insert(name.id, query);
                    name
                }
            };
            self.write(Write::Declaration(declaration), &level, Some(name.clone()));
            level.declare(declaration.name.clone(), name);
        }
        self.nodes(&block.statements, &level)?;
        if !block.handlers.is_empty() {
            let mut section = level;
            let sqlstate = self.name(NameKind::SqlState);
            section.declare("sqlstate", sqlstate);
            let sqlerrm = self.name(NameKind::SqlErrm);
            section.declare("sqlerrm", sqlerrm);
            for handler in &block.handlers {
                self.nodes(handler, &section)?;
            }
        }
        Ok(())
    }

    /// The names the query of `cursor` reads: a level inside `level`, where
    /// the cursor is declared, labelled with the cursor's name and holding
    /// its parameters.
    fn cursor_parameters(
        &mut self,
        level: &Names,
        declaration: &Declaration,
        cursor: &Cursor,
    ) -> Names {
        let mut parameters = Level::new(Some(Cow::Owned(declaration.name.clone())), Some(level));
        for parameter in &cursor.parameters {
            let type_of = self.type_of(&parameters, &parameter.data_type);
            let name = self.name(NameKind::CursorParameter {
                parameter: Arc::new(parameter.clone()),
                type_of,
            });
            parameters.declare(parameter.name.clone(), name);
        }
        parameters
    }

    /// The variable the alias `declaration` is for, looked up where it
    /// stands.
    fn aliased(
        &self,
        level: &Names,
        declaration: &Declaration,
        target: &AliasTarget,
    ) -> Result<Name, Error> {
        let parts = match target {
            AliasTarget::Parameter(position) => Vec::from([format!("${position}")]),
            AliasTarget::Name(name) => stored_parts(name),
        };
        level
            .lookup(&parts, &|name| row(name, self.holds_row))
            .map(|(name, _)| name.clone())
            .ok_or_else(|| Error::UnknownAliasTarget {
                alias: declaration.name.clone(),
                target: match target {
                    AliasTarget::Parameter(position) => format!("${position}"),
                    AliasTarget::Name(name) => name.to_string(),
                },
            })
    }

    /// The variable a `%TYPE` of `data_type` names in `level`.
    ///
    /// Like PostgreSQL, only a name that reaches a variable whole counts.
    fn type_of(&self, level: &Names, data_type: &DeclaredType) -> Option<Box<Name>> {
        let DeclaredType::ColumnType { of, .. } = data_type else {
            return None;
        };
        let parts = stored_parts(of);
        level
            .lookup(&parts, &|name| row(name, self.holds_row))
            .filter(|&(_, consumed)| consumed == parts.len())
            .map(|(name, _)| Box::new(name.clone()))
    }

    fn nodes(&mut self, nodes: &'body [Node], level: &Names) -> Result<(), Error> {
        for node in nodes {
            match node {
                Node::Statement { statement, into } => {
                    self.site(Sql::Statement(statement), level);
                    if into.is_some() || matches!(**statement, Statement::Call(_)) {
                        self.write(Write::Node(node), level, None);
                    }
                }
                Node::Assignment(assignment) => {
                    self.subscripts(&assignment.target, level);
                    self.site(Sql::Expression(&assignment.value), level);
                    self.write(Write::Node(node), level, None);
                }
                Node::Dynamic { dynamic, into } => {
                    self.site(Sql::Dynamic(dynamic), level);
                    if into.is_some() {
                        self.write(Write::Node(node), level, None);
                    }
                }
                Node::Fetch(_) | Node::Diagnostics(_) => {
                    self.write(Write::Node(node), level, None);
                }
                Node::Open(open) => {
                    match open {
                        Open::Bound(cursor) => self.opened(cursor, level)?,
                        Open::Query { query, .. } => self.site(Sql::Statement(query), level),
                        Open::Dynamic { dynamic, .. } => self.site(Sql::Dynamic(dynamic), level),
                    }
                    self.write(Write::Node(node), level, None);
                }
                Node::Block(block) => self.block(block, level)?,
                Node::If(conditional) => {
                    self.branches(&conditional.branches, level)?;
                    self.nodes(&conditional.otherwise, level)?;
                }
                Node::Case(case) => {
                    if let Some(subject) = &case.subject {
                        self.site(Sql::Expression(subject), level);
                    }
                    self.branches(&case.branches, level)?;
                    self.nodes(&case.otherwise, level)?;
                }
                Node::Loop(repeated) => self.looped(node, repeated, level)?,
                Node::Exit(exit) => {
                    if let Some(condition) = &exit.condition {
                        self.site(Sql::Expression(condition), level);
                    }
                }
                Node::Return { value } => {
                    if let Some(value) = value {
                        self.site(Sql::Expression(value), level);
                    }
                }
                Node::Raise(raise) => {
                    let options = raise.options.iter().map(|option| &option.value);
                    for value in raise.arguments.iter().chain(options) {
                        self.site(Sql::Expression(value), level);
                    }
                }
                Node::Assert(assert) => {
                    self.site(Sql::Expression(&assert.condition), level);
                    if let Some(message) = &assert.message {
                        self.site(Sql::Expression(message), level);
                    }
                }
            }
        }
        Ok(())
    }

    /// The subscripts of `target`, in the order written.
    fn subscripts(&mut self, target: &'body Target, level: &Names) {
        for step in &target.indirection {
            match step {
                Indirection::Field(_) => {}
                Indirection::Index(index) => self.site(Sql::Expression(index), level),
                Indirection::Slice { lower, upper } => {
                    for bound in [lower, upper].into_iter().flatten() {
                        self.site(Sql::Expression(bound), level);
                    }
                }
            }
        }
    }

    /// Each branch's condition, then its statements.
    fn branches(&mut self, branches: &'body [Branch], level: &Names) -> Result<(), Error> {
        for branch in branches {
            self.site(Sql::Expression(&branch.condition), level);
            self.nodes(&branch.statements, level)?;
        }
        Ok(())
    }

    /// A loop opens a level, labelled with its label, which holds the
    /// variable a range or cursor `FOR` loop declares once its control is
    /// read. The loop's write reaches the level without that variable.
    fn looped(
        &mut self,
        node: &'body Node,
        repeated: &'body Loop,
        outer: &Names,
    ) -> Result<(), Error> {
        let mut level = Level::new(repeated.label.clone().map(Cow::Owned), Some(outer));
        match &repeated.control {
            LoopControl::Unconditional => {}
            LoopControl::While(expression) => self.site(Sql::Expression(expression), &level),
            LoopControl::Array { array, .. } => {
                self.site(Sql::Expression(array), &level);
                self.write(Write::Node(node), &level, None);
            }
            LoopControl::Range {
                variable,
                lower,
                upper,
                step,
            } => {
                for bound in [Some(lower), Some(upper), step.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    self.site(Sql::Expression(bound), &level);
                }
                let index = self.name(NameKind::LoopIndex {
                    variable: variable.clone(),
                });
                self.write(Write::Node(node), &level, Some(index.clone()));
                level.declare(variable.clone(), index);
            }
            LoopControl::Query { query, .. } => {
                self.site(Sql::Statement(query), &level);
                self.write(Write::Node(node), &level, None);
            }
            LoopControl::Cursor { variable, cursor } => {
                self.opened(cursor, &level)?;
                let record = self.name(NameKind::LoopRecord {
                    variable: variable.clone(),
                    query: Arc::new((*cursor.query).clone()),
                });
                self.write(Write::Node(node), &level, Some(record.clone()));
                level.declare(variable.clone(), record);
            }
            LoopControl::Dynamic { dynamic, .. } => {
                self.site(Sql::Dynamic(dynamic), &level);
                self.write(Write::Node(node), &level, None);
            }
        }
        self.nodes(&repeated.statements, &level)
    }

    /// The arguments of `cursor`, read in `level`, then its query, which
    /// reads the names in reach where the cursor is declared.
    fn opened(&mut self, cursor: &'body BoundCursor, level: &Names) -> Result<(), Error> {
        for argument in &cursor.arguments {
            self.site(Sql::Expression(argument), level);
        }
        let parts = stored_parts(&cursor.name);
        let query = level
            .lookup(&parts, &|name| row(name, self.holds_row))
            .filter(|&(_, consumed)| consumed == parts.len())
            .and_then(|(name, _)| self.cursors.get(&name.id))
            .cloned()
            .ok_or_else(|| Error::UnboundCursor {
                name: parts.join("."),
            })?;
        self.site(Sql::Statement(&cursor.query), &query);
        Ok(())
    }

    fn site(&mut self, sql: Sql<'body>, level: &Names) {
        self.sites.push(Site {
            sql,
            namespace: Namespace {
                level: level.clone(),
            },
        });
    }

    fn write(&mut self, write: Write<'body>, level: &Names, declared: Option<Name>) {
        self.write_sites.push(WriteSite {
            write,
            namespace: Namespace {
                level: level.clone(),
            },
            declared,
        });
    }
}

/// The parts of `name` as PostgreSQL stores them. A part that is no
/// identifier stays empty, which no declaration matches.
pub(crate) fn stored_parts(name: &ObjectName) -> Vec<String> {
    name.0
        .iter()
        .map(|part| part.as_ident().map(stored).unwrap_or_default())
        .collect()
}

/// The name `ident` denotes, as PostgreSQL stores it.
pub(crate) fn stored(ident: &Ident) -> String {
    crate::embedded::folded(&ident.value, ident.quote_style.is_some())
}

/// The levels of names [`Block::sites`] keeps, which own their names so that
/// a [`Namespace`] outlives the body.
type Names = Level<'static, Name>;

/// The names in reach at one point of a body, as PostgreSQL's namespace
/// stack holds them. Each level starts with its label and holds the names
/// declared in it since, inside the levels enclosing it.
///
/// A point shares its entries with the points before it, so keeping the
/// names in reach at each point copies nothing. Both the reader of a body
/// and [`Block::sites`] keep their names as these, each with its own `T`, so
/// both follow one lookup.
pub(crate) struct Level<'name, T> {
    top: Arc<Entry<'name, T>>,
}

/// One entry of the namespace stack, above the entries before it.
struct Entry<'name, T> {
    item: Item<'name, T>,
    below: Option<Arc<Self>>,
}

/// What an entry of the namespace stack holds.
#[derive(Debug)]
enum Item<'name, T> {
    /// The start of a level, with the level's label when it has one.
    Label(Option<Cow<'name, str>>),
    /// A name the level declares.
    Name(Cow<'name, str>, T),
}

impl<'name, T> Level<'name, T> {
    /// A level labelled `label`, inside `outer` when another encloses it.
    pub(crate) fn new(label: Option<Cow<'name, str>>, outer: Option<&Self>) -> Self {
        Self {
            top: Arc::new(Entry {
                item: Item::Label(label),
                below: outer.map(|outer| Arc::clone(&outer.top)),
            }),
        }
    }

    pub(crate) fn declare(&mut self, name: impl Into<Cow<'name, str>>, item: T) {
        let below = Arc::clone(&self.top);
        self.top = Arc::new(Entry {
            item: Item::Name(name.into(), item),
            below: Some(below),
        });
    }

    /// What `plpgsql_ns_lookup` answers for `parts`, as
    /// [`Namespace::lookup`] describes.
    pub(crate) fn lookup<S: AsRef<str>>(
        &self,
        parts: &[S],
        holds_row: &dyn Fn(&T) -> bool,
    ) -> Option<(&T, usize)> {
        let (first, second, third) = match parts {
            [first] => (first.as_ref(), None, false),
            [first, second] => (first.as_ref(), Some(second.as_ref()), false),
            [first, second, _] => (first.as_ref(), Some(second.as_ref()), true),
            _ => return None,
        };
        let mut level = Some(&*self.top);
        while let Some(top) = level {
            if let Some(item) = top.named(first, second.is_some(), holds_row) {
                return Some((item, 1));
            }
            let start = top.start();
            if let Some(second) = second
                && let Some(Entry {
                    item: Item::Label(Some(label)),
                    ..
                }) = start
                && **label == *first
                && let Some(item) = top.named(second, third, holds_row)
            {
                return Some((item, 2));
            }
            level = start.and_then(|start| start.below.as_deref());
        }
        None
    }
}

impl<T> Clone for Level<'_, T> {
    fn clone(&self) -> Self {
        Self {
            top: Arc::clone(&self.top),
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Level<'_, T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.top.entries().map(|entry| &entry.item))
            .finish()
    }
}

impl<T> Entry<'_, T> {
    /// This entry and every entry below it, innermost first.
    fn entries(&self) -> impl Iterator<Item = &Self> {
        core::iter::successors(Some(self), |entry| entry.below.as_deref())
    }

    /// The names the level of this entry declares up to it, latest first.
    fn names(&self) -> impl Iterator<Item = (&str, &T)> {
        self.entries().map_while(|entry| match &entry.item {
            Item::Name(name, item) => Some((&**name, item)),
            Item::Label(_) => None,
        })
    }

    /// The entry that starts the level of this entry.
    fn start(&self) -> Option<&Self> {
        self.entries()
            .find(|entry| matches!(entry.item, Item::Label(_)))
    }

    /// The latest item the level of this entry declares under `name` up to
    /// it, passing over one that holds no row when `field` says a field of
    /// it is read.
    fn named(&self, name: &str, field: bool, holds_row: &dyn Fn(&T) -> bool) -> Option<&T> {
        self.names()
            .find(|&(held, item)| held == name && (!field || holds_row(item)))
            .map(|(_, item)| item)
    }
}

impl<T> Drop for Entry<'_, T> {
    /// Unlinks the entries below one at a time, so that dropping a long
    /// stack takes no stack frame per entry.
    fn drop(&mut self) {
        let mut below = self.below.take();
        while let Some(entry) = below {
            below = Arc::into_inner(entry).and_then(|mut entry| entry.below.take());
        }
    }
}
