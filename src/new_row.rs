//! What a `BEFORE` row trigger leaves in each field of `NEW`, read from a
//! body that only assigns fields of `NEW` and then returns it.
//!
//! Such a body computes one expression per field it assigns. PostgreSQL runs
//! its statements in order, an `IF` runs the first branch whose condition
//! holds, a field no branch assigns keeps its value, and a later read of
//! `NEW.field` sees what an earlier statement assigned.
//! [`Block::fields_of_new`] folds the body into those expressions, each of
//! which reads `NEW` as it was before the body ran.

use alloc::{boxed::Box, string::String, vec, vec::Vec};
use core::{borrow::Borrow, ops::ControlFlow, slice};

use sqlparser::ast::{
    CaseWhen, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName,
    ObjectNamePart, Select, SelectItem, SelectItemQualifiedWildcardKind, VisitMut, VisitorMut,
    helpers::attached_token::AttachedToken,
};

use crate::{
    embedded::{Assignment, Block, Branch, If, Node, expression},
    namespace::{NameKind, Namespace, Signature, TriggerVariable, stored},
};

/// A field of `NEW` and the expression it holds when the body returns.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldValue {
    /// The field, as the body first writes it.
    ///
    /// Two spellings name one field when PostgreSQL stores them alike, so
    /// `NEW.Total` and `NEW.total` assign the same field and `NEW."Total"`
    /// another.
    pub field: Ident,
    /// The expression the field holds, reading `NEW` as it was before the
    /// body ran.
    pub value: Expr,
}

/// Each field assigned so far, under its name as PostgreSQL stores it, in
/// the order first assigned.
type State = Vec<(String, FieldValue)>;

impl Block {
    /// The fields of `NEW` the body this block reads assigns, each with the
    /// expression it holds when the body returns `NEW`, in the order first
    /// assigned.
    ///
    /// `signature` describes the trigger function the body belongs to. The
    /// body must declare nothing, handle no error and end with `RETURN NEW`,
    /// its only `RETURN`, and each statement before it must assign one field
    /// of `NEW` or be an `IF` whose branches hold only such statements. `NEW`
    /// is any name that reaches the trigger's `NEW`, the function's name
    /// qualifying it included.
    ///
    /// An `IF` folds each field a branch changes into a `CASE` over the
    /// conditions in order, whose `ELSE` is the field's value before the
    /// `IF`. A later read of a field reads the expression assigned to it, in
    /// parentheses unless it delimits itself. A read of the whole row `NEW`
    /// after a field is assigned has no such expression.
    ///
    /// `None` for every other body, and for one that assigns nothing.
    ///
    /// # Example
    ///
    /// ```
    /// use sqlparser_plpgsql::{
    ///     embedded_sql,
    ///     namespace::{FunctionKind, Signature},
    /// };
    ///
    /// let block = embedded_sql(
    ///     "BEGIN
    ///        IF NEW.total < 0 THEN NEW.total := 0; END IF;
    ///        NEW.label := NEW.total || ' items';
    ///        RETURN NEW;
    ///      END",
    /// )?;
    /// let signature = Signature {
    ///     name: "clamp".into(),
    ///     parameters: Vec::new(),
    ///     kind: FunctionKind::Trigger,
    /// };
    /// let fields = block.fields_of_new(&signature).expect("the body only assigns NEW");
    /// assert_eq!(fields[0].field.value, "total");
    /// assert_eq!(
    ///     fields[1].value.to_string(),
    ///     "CASE WHEN NEW.total < 0 THEN 0 ELSE NEW.total END || ' items'",
    /// );
    /// # Ok::<(), sqlparser_plpgsql::embedded::Error>(())
    /// ```
    #[must_use]
    pub fn fields_of_new(&self, signature: &Signature) -> Option<Vec<FieldValue>> {
        if !self.declarations.is_empty() || !self.handlers.is_empty() {
            return None;
        }
        let Some((
            Node::Return {
                value: Some(returned),
            },
            statements,
        )) = self.statements.split_last()
        else {
            return None;
        };
        let fold = Fold {
            namespace: Namespace::body(self, signature),
        };
        let returns_new = match &*expression(returned) {
            Expr::Identifier(name) => fold.reaches_new(slice::from_ref(name)),
            Expr::CompoundIdentifier(parts) => fold.reaches_new(parts),
            _ => false,
        };
        if !returns_new {
            return None;
        }
        let mut state = State::new();
        fold.run(statements, &mut state)?;
        (!state.is_empty()).then(|| state.into_iter().map(|(_, assigned)| assigned).collect())
    }
}

/// Folds the statements of a body that only assigns fields of `NEW`.
struct Fold {
    /// The names every statement of the body reaches.
    namespace: Namespace,
}

/// What a name read after some fields of `NEW` are assigned reads.
enum Read<'state> {
    /// A value the body has not assigned.
    Unassigned,
    /// The value assigned to one field.
    Assigned(&'state FieldValue),
    /// The whole row `NEW`, or a part of an assigned field, which no
    /// expression over `NEW` before the body ran reads.
    Unfoldable,
}

impl Fold {
    /// The parts of the name `parts` writes after those that reach `NEW`, or
    /// `None` when it does not start with `NEW`.
    fn after_new<'parts, P: Borrow<Ident>>(&self, parts: &'parts [P]) -> Option<&'parts [P]> {
        let stored: Vec<_> = parts.iter().map(|part| stored(part.borrow())).collect();
        let (name, consumed) = self.namespace.lookup(&stored, |_| false)?;
        matches!(name.kind(), NameKind::Trigger(TriggerVariable::New)).then(|| &parts[consumed..])
    }

    /// Whether the name `parts` is the whole row `NEW`.
    fn reaches_new(&self, parts: &[Ident]) -> bool {
        self.after_new(parts).is_some_and(<[Ident]>::is_empty)
    }

    /// What the name `parts` reads once `state` is assigned.
    fn read<'state, P: Borrow<Ident>>(&self, parts: &[P], state: &'state State) -> Read<'state> {
        let Some(fields) = self.after_new(parts) else {
            return Read::Unassigned;
        };
        let [field, rest @ ..] = fields else {
            return if state.is_empty() {
                Read::Unassigned
            } else {
                Read::Unfoldable
            };
        };
        match assigned(state, &stored(field.borrow())) {
            None => Read::Unassigned,
            Some(value) if rest.is_empty() => Read::Assigned(value),
            Some(_) => Read::Unfoldable,
        }
    }

    /// Whether the wildcard `prefix.*` reads anything assigned in `state`.
    fn expands_assigned(&self, prefix: &ObjectName, state: &State) -> bool {
        let parts: Option<Vec<&Ident>> = prefix.0.iter().map(ObjectNamePart::as_ident).collect();
        parts.is_some_and(|parts| !matches!(self.read(&parts, state), Read::Unassigned))
    }

    /// Runs `statements` over `state`, or `None` when one is not an
    /// assignment to a field of `NEW` or an `IF` over such statements.
    fn run(&self, statements: &[Node], state: &mut State) -> Option<()> {
        for statement in statements {
            match statement {
                Node::Assignment(assignment) => self.assign(assignment, state)?,
                Node::If(conditional) => self.branch(conditional, state)?,
                _ => return None,
            }
        }
        Some(())
    }

    /// Runs `assignment` over `state`, refusing one to anything but a field
    /// of `NEW`.
    fn assign(&self, assignment: &Assignment, state: &mut State) -> Option<()> {
        let target = &assignment.target;
        if !target.indirection.is_empty() {
            return None;
        }
        let parts: Vec<&Ident> = target
            .name
            .0
            .iter()
            .map(ObjectNamePart::as_ident)
            .collect::<Option<_>>()?;
        let [field] = self.after_new(&parts)? else {
            return None;
        };
        let value = self.substituted(expression(&assignment.value).into_owned(), state)?;
        set(state, field, value);
        Some(())
    }

    /// Runs `conditional` over `state`, folding each field a branch changes
    /// into a `CASE` over the conditions of the branches.
    fn branch(&self, conditional: &If, state: &mut State) -> Option<()> {
        let mut arms = Vec::with_capacity(conditional.branches.len());
        for Branch {
            condition,
            statements,
        } in &conditional.branches
        {
            let condition = self.substituted(expression(condition).into_owned(), state)?;
            let mut arm = state.clone();
            self.run(statements, &mut arm)?;
            arms.push((condition, arm));
        }
        let mut otherwise = state.clone();
        self.run(&conditional.otherwise, &mut otherwise)?;

        let mut fields: Vec<(&str, &Ident)> = Vec::new();
        for (key, assigned) in arms
            .iter()
            .map(|(_, arm)| arm)
            .chain([&otherwise])
            .flatten()
        {
            if !fields.iter().any(|(seen, _)| *seen == key) {
                fields.push((key, &assigned.field));
            }
        }
        let mut folded = Vec::new();
        for (key, field) in fields {
            let before = value_of(state, key, field);
            let after = |arm: &State| value_of(arm, key, field);
            if after(&otherwise) == before && arms.iter().all(|(_, arm)| after(arm) == before) {
                continue;
            }
            let conditions = arms
                .iter()
                .map(|(condition, arm)| CaseWhen {
                    condition: condition.clone(),
                    result: after(arm),
                })
                .collect();
            let case = Expr::Case {
                case_token: AttachedToken::empty(),
                end_token: AttachedToken::empty(),
                operand: None,
                conditions,
                else_result: Some(Box::new(after(&otherwise))),
            };
            folded.push((field, case));
        }
        for (field, case) in folded {
            set(state, field, case);
        }
        Some(())
    }

    /// `expression` with every field of `NEW` it reads replaced by the value
    /// `state` holds for it, or `None` when it reads `NEW` in a way no
    /// replacement expresses.
    fn substituted(&self, mut expression: Expr, state: &State) -> Option<Expr> {
        let mut reads = Reads { fold: self, state };
        match expression.visit(&mut reads) {
            ControlFlow::Continue(()) => Some(expression),
            ControlFlow::Break(()) => None,
        }
    }
}

/// Replaces each read of an assigned field of `NEW` by its value, once the
/// read's own subexpressions are replaced, and stops at a read no value
/// replaces.
struct Reads<'fold> {
    /// The fold reading the body.
    fold: &'fold Fold,
    /// The fields assigned so far.
    state: &'fold State,
}

impl VisitorMut for Reads<'_> {
    type Break = ();

    fn pre_visit_select(&mut self, select: &mut Select) -> ControlFlow<()> {
        let expands = select.projection.iter().any(|item| {
            matches!(
                item,
                SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(prefix), _)
                    if self.fold.expands_assigned(prefix, self.state)
            )
        });
        if expands {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }

    fn post_visit_expr(&mut self, read: &mut Expr) -> ControlFlow<()> {
        let parts = match read {
            Expr::Identifier(name) => slice::from_ref(name),
            Expr::CompoundIdentifier(parts) => parts.as_slice(),
            Expr::QualifiedWildcard(prefix, _) => {
                return if self.fold.expands_assigned(prefix, self.state) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                };
            }
            Expr::Function(function) => {
                let FunctionArguments::List(list) = &function.args else {
                    return ControlFlow::Continue(());
                };
                let expands = list.args.iter().any(|argument| {
                    let (FunctionArg::Unnamed(argument)
                    | FunctionArg::Named { arg: argument, .. }
                    | FunctionArg::ExprNamed { arg: argument, .. }) = argument;
                    matches!(
                        argument,
                        FunctionArgExpr::QualifiedWildcard(prefix)
                            if self.fold.expands_assigned(prefix, self.state)
                    )
                });
                return if expands {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                };
            }
            _ => return ControlFlow::Continue(()),
        };
        match self.fold.read(parts, self.state) {
            Read::Unassigned => ControlFlow::Continue(()),
            Read::Assigned(assigned) => {
                *read = parenthesized(assigned.value.clone());
                ControlFlow::Continue(())
            }
            Read::Unfoldable => ControlFlow::Break(()),
        }
    }
}

/// `value` in parentheses unless it delimits itself, so that it binds as one
/// operand where it replaces a read.
fn parenthesized(value: Expr) -> Expr {
    match value {
        Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Value(_)
        | Expr::Function(_)
        | Expr::Nested(_)
        | Expr::Case { .. } => value,
        _ => Expr::Nested(Box::new(value)),
    }
}

/// What `state` assigns to the field PostgreSQL stores as `key`.
fn assigned<'state>(state: &'state State, key: &str) -> Option<&'state FieldValue> {
    state
        .iter()
        .find_map(|(assigned, value)| (assigned == key).then_some(value))
}

/// The value the field `field`, stored as `key`, holds in `state`, which is
/// `NEW.field` when nothing assigned it.
fn value_of(state: &State, key: &str, field: &Ident) -> Expr {
    assigned(state, key).map_or_else(
        || Expr::CompoundIdentifier(vec![Ident::new("NEW"), field.clone()]),
        |assigned| assigned.value.clone(),
    )
}

/// Records that `field` holds `value` in `state`.
fn set(state: &mut State, field: &Ident, value: Expr) {
    let key = stored(field);
    match state.iter_mut().find(|(assigned, _)| *assigned == key) {
        Some((_, held)) => held.value = value,
        None => state.push((
            key,
            FieldValue {
                field: field.clone(),
                value,
            },
        )),
    }
}
