//! Which variable a name written in a PL/pgSQL body reaches, as PostgreSQL's
//! `plpgsql_ns_lookup` answers it.
//!
//! PL/pgSQL nests levels of names. The outermost belongs to the function,
//! labelled with the function's name, and holds its parameters, the variables
//! a trigger function receives and `FOUND`. Inside it each block opens a
//! level for its declarations, labelled with the block's label, and each loop
//! opens one for the variable a `FOR` loop declares. An `EXCEPTION` section
//! adds `SQLSTATE` and `SQLERRM` to its block's level.

use alloc::{
    borrow::Cow,
    boxed::Box,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::hash::{Hash, Hasher};

use sqlparser::ast::{Ident, ObjectName, Statement};

use crate::embedded::{
    AliasTarget, Block, Declaration, DeclarationKind, DeclaredType, Dynamic, Loop, LoopControl,
    Node,
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
            NameKind::Parameter { .. } => None,
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
            | NameKind::LoopIndex
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
    /// The integer a `FOR i IN lower .. upper` loop declares for its
    /// statements.
    LoopIndex,
    /// The record a `FOR r IN cursor` loop declares for its statements.
    LoopRecord {
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
    level: Arc<Names>,
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
    /// `holds_row` answers for a parameter or a declared variable, whose type
    /// decides. Every other variable holds a row only when it is a loop's
    /// record, `NEW` or `OLD`.
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

/// A place a body runs SQL it builds at run time, with the names in reach
/// there.
#[derive(Clone, Debug)]
pub struct DynamicSite<'body> {
    /// The command and its `USING` arguments.
    pub dynamic: &'body Dynamic,
    /// The names the command and its arguments reach.
    pub namespace: Namespace,
}

impl Block {
    /// Every `EXECUTE`, `RETURN QUERY EXECUTE`, `OPEN ... FOR EXECUTE` and
    /// `FOR ... IN EXECUTE` of the body this block reads, in body order,
    /// with the names in reach at each.
    ///
    /// `holds_row` answers whether a parameter or a declared variable holds
    /// a row, as for [`Namespace::lookup`], which an alias or a `%TYPE`
    /// naming a field needs.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownAliasTarget`] when `ALIAS FOR` names no
    /// variable in reach.
    ///
    /// # Example
    ///
    /// ```
    /// use sqlparser_plpgsql::{
    ///     embedded_sql,
    ///     namespace::{FunctionKind, NameKind, Signature},
    /// };
    ///
    /// let block = embedded_sql("DECLARE p integer; BEGIN EXECUTE 'SELECT ' || f.p; END")?;
    /// let signature = Signature {
    ///     name: "f".into(),
    ///     parameters: vec![Some("p".into())],
    ///     kind: FunctionKind::Plain,
    /// };
    /// let sites = block
    ///     .dynamic_sites(&signature, |_| false)
    ///     .expect("the body declares no alias");
    /// let (name, consumed) = sites[0]
    ///     .namespace
    ///     .lookup(&["f", "p"], |_| false)
    ///     .expect("f.p reaches the parameter");
    /// assert_eq!(name.kind(), &NameKind::Parameter { position: 1 });
    /// assert_eq!(consumed, 2);
    /// # Ok::<(), sqlparser_plpgsql::embedded::Error>(())
    /// ```
    pub fn dynamic_sites<'body>(
        &'body self,
        signature: &Signature,
        holds_row: impl Fn(&Name) -> bool,
    ) -> Result<Vec<DynamicSite<'body>>, Error> {
        let mut collector = Collector {
            holds_row: &holds_row,
            next_id: 0,
            sites: Vec::new(),
        };
        let function = Arc::new(collector.function_level(signature));
        collector.block(self, &function)?;
        Ok(collector.sites)
    }
}

/// Walks a body in order, numbering its variables and recording each
/// dynamic site with the level in force there.
struct Collector<'body, 'predicate> {
    holds_row: &'predicate dyn Fn(&Name) -> bool,
    next_id: usize,
    sites: Vec<DynamicSite<'body>>,
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

    fn block(&mut self, block: &'body Block, parent: &Arc<Names>) -> Result<(), Error> {
        let mut level = Level::new(
            block.label.clone().map(Cow::Owned),
            Some(Arc::clone(parent)),
        );
        for declaration in &block.declarations {
            let name = match &declaration.kind {
                DeclarationKind::Alias(target) => self.aliased(&level, declaration, target)?,
                DeclarationKind::Variable(variable) => {
                    let type_of = self.type_of(&level, &variable.data_type);
                    self.name(NameKind::Declared {
                        declaration: Arc::new(declaration.clone()),
                        type_of,
                    })
                }
                DeclarationKind::Cursor(_) => self.name(NameKind::Declared {
                    declaration: Arc::new(declaration.clone()),
                    type_of: None,
                }),
            };
            level.declare(declaration.name.clone(), name);
        }
        let level = Arc::new(level);
        self.nodes(&block.statements, &level)?;
        if !block.handlers.is_empty() {
            let mut section = level.extended();
            let sqlstate = self.name(NameKind::SqlState);
            section.declare("sqlstate", sqlstate);
            let sqlerrm = self.name(NameKind::SqlErrm);
            section.declare("sqlerrm", sqlerrm);
            let section = Arc::new(section);
            for handler in &block.handlers {
                self.nodes(handler, &section)?;
            }
        }
        Ok(())
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

    fn nodes(&mut self, nodes: &'body [Node], level: &Arc<Names>) -> Result<(), Error> {
        for node in nodes {
            match node {
                Node::Statement(_) => {}
                Node::Dynamic(dynamic) => self.site(dynamic, level),
                Node::Block(block) => self.block(block, level)?,
                Node::If(conditional) => {
                    for branch in &conditional.branches {
                        self.nodes(&branch.statements, level)?;
                    }
                    self.nodes(&conditional.otherwise, level)?;
                }
                Node::Case(case) => {
                    for branch in &case.branches {
                        self.nodes(&branch.statements, level)?;
                    }
                    self.nodes(&case.otherwise, level)?;
                }
                Node::Loop(repeated) => self.looped(repeated, level)?,
            }
        }
        Ok(())
    }

    /// A loop opens a level, labelled with its label, which holds the
    /// variable a range or cursor `FOR` loop declares.
    fn looped(&mut self, repeated: &'body Loop, parent: &Arc<Names>) -> Result<(), Error> {
        let mut level = Level::new(
            repeated.label.clone().map(Cow::Owned),
            Some(Arc::clone(parent)),
        );
        match &repeated.control {
            LoopControl::Range { variable, .. } => {
                let index = self.name(NameKind::LoopIndex);
                level.declare(variable.clone(), index);
            }
            LoopControl::Cursor {
                variable, query, ..
            } => {
                let record = self.name(NameKind::LoopRecord {
                    query: Arc::new((**query).clone()),
                });
                level.declare(variable.clone(), record);
            }
            LoopControl::Unconditional
            | LoopControl::While(_)
            | LoopControl::Query(_)
            | LoopControl::Dynamic(_)
            | LoopControl::Array(_) => {}
        }
        let level = Arc::new(level);
        if let LoopControl::Dynamic(dynamic) = &repeated.control {
            self.site(dynamic, &level);
        }
        self.nodes(&repeated.statements, &level)
    }

    fn site(&mut self, dynamic: &'body Dynamic, level: &Arc<Names>) {
        self.sites.push(DynamicSite {
            dynamic,
            namespace: Namespace {
                level: Arc::clone(level),
            },
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
fn stored(ident: &Ident) -> String {
    crate::embedded::folded(&ident.value, ident.quote_style.is_some())
}

/// The levels of names [`Block::dynamic_sites`] keeps, which own their names
/// so that a [`Namespace`] outlives the body.
type Names = Level<'static, Name>;

/// One level of names, inside the level enclosing it.
///
/// Both the reader of a body and [`Block::dynamic_sites`] keep their levels
/// as these, each with its own `T`, so both follow one lookup.
#[derive(Debug)]
pub(crate) struct Level<'name, T> {
    label: Option<Cow<'name, str>>,
    names: Vec<(Cow<'name, str>, T)>,
    parent: Option<Arc<Level<'name, T>>>,
}

impl<'name, T> Level<'name, T> {
    pub(crate) const fn new(label: Option<Cow<'name, str>>, parent: Option<Arc<Self>>) -> Self {
        Self {
            label,
            names: Vec::new(),
            parent,
        }
    }

    pub(crate) fn declare(&mut self, name: impl Into<Cow<'name, str>>, item: T) {
        self.names.push((name.into(), item));
    }

    /// Whether this level itself declares `name`.
    pub(crate) fn declares(&self, name: &str) -> bool {
        self.names.iter().any(|(held, _)| held == name)
    }

    /// A copy of this level to declare more names in, for the handlers of an
    /// `EXCEPTION` section.
    pub(crate) fn extended(&self) -> Self
    where
        T: Clone,
    {
        Self {
            label: self.label.clone(),
            names: self.names.clone(),
            parent: self.parent.clone(),
        }
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
        let mut level = Some(self);
        while let Some(current) = level {
            if let Some(item) = current.named(first, second.is_some(), holds_row) {
                return Some((item, 1));
            }
            if let Some(second) = second
                && current.label.as_deref() == Some(first)
                && let Some(item) = current.named(second, third, holds_row)
            {
                return Some((item, 2));
            }
            level = current.parent.as_deref();
        }
        None
    }

    /// The latest item this level declares under `name`, passing over one
    /// that holds no row when `field` says a field of it is read.
    fn named(&self, name: &str, field: bool, holds_row: &dyn Fn(&T) -> bool) -> Option<&T> {
        self.names
            .iter()
            .rev()
            .find(|(held, item)| held == name && (!field || holds_row(item)))
            .map(|(_, item)| item)
    }
}
