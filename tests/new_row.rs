//! What a trigger body that only assigns fields of `NEW` leaves in each of
//! them, and what refuses any other body, read through the public API.
#![cfg(feature = "body-parse")]

mod common;

use common::quoted;
use sqlparser::{
    ast::{Expr, Spanned},
    tokenizer::Span,
};
use sqlparser_plpgsql::{
    embedded::Block,
    embedded_sql,
    namespace::{FunctionKind, Signature},
    new_row::NotFolded,
};

/// The body of the trigger function `maintain()`.
fn read(body: &str) -> Block {
    embedded_sql(body).unwrap_or_else(|error| panic!("{body}: {error}"))
}

fn maintain() -> Signature {
    Signature {
        name: "maintain".into(),
        parameters: Vec::new(),
        kind: FunctionKind::Trigger,
    }
}

/// The fields `body` leaves in `NEW`, each written as its field and the
/// expression it holds.
fn fields_of_body(body: &str) -> Vec<(String, String)> {
    read(body)
        .fields_of_new(&maintain())
        .unwrap_or_else(|refusal| panic!("{body}: {refusal}"))
        .into_iter()
        .map(|assigned| (assigned.field.to_string(), assigned.value.to_string()))
        .collect()
}

/// What refuses `body`, as the name of the refusal and the text `body`
/// writes for what it carries, the read before its statement.
fn refusal_of_body(body: &str) -> (&'static str, Vec<String>) {
    let block = read(body);
    let refusal = match block.fields_of_new(&maintain()) {
        Ok(fields) => panic!("{body} folds into {fields:?}"),
        Err(refusal) => refusal,
    };
    let quote = |span| String::from(quoted(body, span));
    match refusal {
        NotFolded::Declaration(declaration) => ("declaration", vec![declaration.name.clone()]),
        NotFolded::Statement(node) => ("statement", vec![quote(node.span)]),
        NotFolded::EarlyReturn(node) => ("early return", vec![quote(node.span)]),
        NotFolded::NoReturnOfNew(node) => ("no return of NEW", vec![quote(node.span)]),
        NotFolded::NoStatement => ("no statement", Vec::new()),
        NotFolded::Read { statement, read } => {
            let span = read.span();
            let text = if let Expr::Function(_) = *read {
                // sqlparser's span of a call stops before its `)`, which
                // apache/datafusion-sqlparser-rs#2723 keeps, so the call is
                // checked by where it starts and by its text.
                let call = read.to_string();
                let from_start = quoted(body, Span::new(span.start, statement.span.end));
                assert!(
                    from_start.starts_with(&call),
                    "{from_start} starts with {call}"
                );
                call
            } else {
                quote(span)
            };
            ("read", vec![text, quote(statement.span)])
        }
        NotFolded::Handler(_) => ("handler", Vec::new()),
        NotFolded::NothingAssigned => ("nothing assigned", Vec::new()),
    }
}

/// The fields `statements` leave in `NEW` when `RETURN NEW` follows them.
fn fields(statements: &str) -> Vec<(String, String)> {
    fields_of_body(&format!("BEGIN {statements} RETURN NEW; END"))
}

/// What refuses `statements` when `RETURN NEW` follows them.
fn refusal(statements: &str) -> (&'static str, Vec<String>) {
    refusal_of_body(&format!("BEGIN {statements} RETURN NEW; END"))
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|&(field, value)| (field.into(), value.into()))
        .collect()
}

fn texts(expected: &[&str]) -> Vec<String> {
    expected.iter().map(|&text| text.into()).collect()
}

#[test]
fn a_nested_if_folds_into_a_nested_case() {
    assert_eq!(
        fields(
            "IF NEW.archived_at IS NOT NULL THEN
               IF TG_OP = 'UPDATE' AND OLD.archived_at IS NOT NULL THEN
                 NEW.archived_at := OLD.archived_at;
               ELSE
                 NEW.archived_at := now();
               END IF;
             END IF;"
        ),
        pairs(&[(
            "archived_at",
            "CASE WHEN NEW.archived_at IS NOT NULL THEN CASE WHEN TG_OP = 'UPDATE' AND \
             OLD.archived_at IS NOT NULL THEN OLD.archived_at ELSE now() END ELSE \
             NEW.archived_at END",
        )])
    );
}

#[test]
fn an_elsif_chain_without_else_keeps_the_value_before_the_if() {
    assert_eq!(
        fields("IF NEW.a < 0 THEN NEW.a := 0; ELSIF NEW.a > 10 THEN NEW.a := 10; END IF;"),
        pairs(&[(
            "a",
            "CASE WHEN NEW.a < 0 THEN 0 WHEN NEW.a > 10 THEN 10 ELSE NEW.a END"
        )])
    );
}

#[test]
fn fields_assigned_in_different_branches_each_get_a_case() {
    assert_eq!(
        fields(
            "IF TG_OP = 'INSERT' THEN NEW.created_at := now(); \
             ELSE NEW.updated_at := now(); END IF;"
        ),
        pairs(&[
            (
                "created_at",
                "CASE WHEN TG_OP = 'INSERT' THEN now() ELSE NEW.created_at END"
            ),
            (
                "updated_at",
                "CASE WHEN TG_OP = 'INSERT' THEN NEW.updated_at ELSE now() END"
            ),
        ])
    );
}

#[test]
fn a_read_after_an_assignment_reads_the_assigned_value() {
    assert_eq!(
        fields("NEW.a := NEW.a + 1; NEW.b := NEW.a * 2;"),
        pairs(&[("a", "NEW.a + 1"), ("b", "(NEW.a + 1) * 2")])
    );
    assert_eq!(
        fields("NEW.a := 1; IF NEW.a > 0 THEN NEW.b := NEW.a; END IF;"),
        pairs(&[("a", "1"), ("b", "CASE WHEN 1 > 0 THEN 1 ELSE NEW.b END")])
    );
    assert_eq!(
        fields("IF TG_OP = 'INSERT' THEN NEW.a := 1; END IF; NEW.a := NEW.a + 1;"),
        pairs(&[("a", "CASE WHEN TG_OP = 'INSERT' THEN 1 ELSE NEW.a END + 1")])
    );
    assert_eq!(
        fields("NEW.a := 1; NEW.b := (SELECT x FROM t WHERE t.id = NEW.a);"),
        pairs(&[("a", "1"), ("b", "(SELECT x FROM t WHERE t.id = 1)")])
    );
    assert_eq!(
        fields("NEW.a := 1; NEW.b := CURRENT_TIMESTAMP;"),
        pairs(&[("a", "1"), ("b", "CURRENT_TIMESTAMP")])
    );
}

#[test]
fn a_field_is_the_name_postgresql_stores_and_keeps_its_first_spelling() {
    assert_eq!(
        fields("NEW.Total := 1; NEW.total := NEW.TOTAL + 1;"),
        pairs(&[("Total", "1 + 1")])
    );
    assert_eq!(
        fields("NEW.\"ColA\" := 1; NEW.b := NEW.\"ColA\"; NEW.c := NEW.cola;"),
        pairs(&[("\"ColA\"", "1"), ("b", "1"), ("c", "NEW.cola")])
    );
}

#[test]
fn new_qualified_by_the_function_name_is_new() {
    assert_eq!(
        fields_of_body(
            "BEGIN maintain.NEW.a := 1; NEW.b := maintain.new.a + 1; RETURN maintain.NEW; END"
        ),
        pairs(&[("a", "1"), ("b", "1 + 1")])
    );
    assert_eq!(
        refusal("NEW.a := 1; NEW.h := md5(maintain.NEW::text);"),
        (
            "read",
            texts(&["maintain.NEW", "NEW.h := md5(maintain.NEW::text);"])
        )
    );
}

#[test]
fn the_whole_row_is_read_only_before_any_assignment() {
    for (expression, read) in [
        ("md5(NEW::text)", "NEW"),
        ("md5(ROW(NEW.*)::text)", "ROW(NEW.*)"),
        ("md5(row_to_json(NEW.*)::text)", "row_to_json(NEW.*)"),
        ("md5((0, NEW.*)::text)", "NEW.*"),
        ("(SELECT md5(r::text) FROM (SELECT NEW.*) AS r)", "NEW.*"),
    ] {
        let before = fields(&format!("NEW.h := {expression}; NEW.a := 1;"));
        assert_eq!(before[1], ("a".into(), "1".into()), "{expression}");
        let assignment = format!("NEW.h := {expression};");
        assert_eq!(
            refusal(&format!("NEW.a := 1; {assignment}")),
            ("read", texts(&[read, &assignment])),
            "{expression}"
        );
    }
    assert_eq!(
        fields("NEW.h := md5(NEW::text); NEW.a := 1;"),
        pairs(&[("h", "md5(NEW::TEXT)"), ("a", "1")])
    );
    let expands_a = "NEW.b := ROW(NEW.a.*); NEW.a := ROW(1);";
    assert_eq!(fields(expands_a).len(), 2, "{expands_a}");
    assert_eq!(
        refusal(&format!("{expands_a} NEW.c := ROW(NEW.a.*);")),
        ("read", texts(&["ROW(NEW.a.*)", "NEW.c := ROW(NEW.a.*);"]))
    );
    let reads_a_field = "NEW.b := NEW.a.f1; NEW.a := ROW(1);";
    assert_eq!(fields(reads_a_field).len(), 2, "{reads_a_field}");
    assert_eq!(
        refusal(&format!("{reads_a_field} NEW.c := NEW.a.f1;")),
        ("read", texts(&["NEW.a.f1", "NEW.c := NEW.a.f1;"]))
    );
    assert_eq!(
        refusal("NEW.a := 1; IF NEW IS NULL THEN NEW.b := 2; END IF;"),
        (
            "read",
            texts(&["NEW", "IF NEW IS NULL THEN NEW.b := 2; END IF;"])
        )
    );
}

#[test]
fn any_other_statement_refuses_the_body_as_written() {
    for statement in [
        "RAISE NOTICE 'stamping';",
        "PERFORM pg_notify('t', 'stamped');",
        "INSERT INTO t (a) VALUES (1);",
        "LOOP EXIT; END LOOP;",
        "<<inner>> BEGIN NEW.a := 2; END;",
        "NEW.a[1] := 2;",
        "NEW.a.b := 2;",
        "OLD.a := 1;",
    ] {
        assert_eq!(
            refusal(&format!("NEW.a := 1; {statement}")),
            ("statement", texts(&[statement])),
            "{statement}"
        );
    }
    assert_eq!(
        refusal("NEW.a := 1; IF NEW.a IS NULL THEN RAISE 'no a'; END IF;"),
        ("statement", texts(&["RAISE 'no a';"]))
    );
}

#[test]
fn a_return_before_the_end_refuses_the_body() {
    assert_eq!(
        refusal("NEW.a := 1; IF NEW.b IS NULL THEN RETURN NEW; END IF;"),
        ("early return", texts(&["RETURN NEW;"]))
    );
    assert_eq!(
        refusal("NEW.a := 1; RETURN NULL;"),
        ("early return", texts(&["RETURN NULL;"]))
    );
}

#[test]
fn a_body_that_does_not_end_returning_new_is_refused_at_its_last_statement() {
    assert_eq!(
        refusal_of_body("BEGIN NEW.a := 1; RETURN NULL; END"),
        ("no return of NEW", texts(&["RETURN NULL;"]))
    );
    assert_eq!(
        refusal_of_body("BEGIN NEW.a := 1; NEW.b := 2; END"),
        ("no return of NEW", texts(&["NEW.b := 2;"]))
    );
    assert_eq!(
        refusal_of_body("BEGIN NEW.a := 1; INSERT INTO t VALUES (1); END"),
        ("statement", texts(&["INSERT INTO t VALUES (1);"]))
    );
    assert_eq!(
        refusal_of_body("BEGIN NULL; END"),
        ("no statement", Vec::new())
    );
}

#[test]
fn declarations_handlers_and_a_body_assigning_nothing_are_refused() {
    assert_eq!(
        refusal_of_body("DECLARE x int; BEGIN NEW.a := 1; RETURN NEW; END"),
        ("declaration", texts(&["x"]))
    );
    assert_eq!(
        refusal_of_body("BEGIN NEW.a := 1; RETURN NEW; EXCEPTION WHEN OTHERS THEN RETURN NEW; END"),
        ("handler", Vec::new())
    );
    assert_eq!(
        refusal_of_body("BEGIN RETURN NEW; END"),
        ("nothing assigned", Vec::new())
    );
}

#[test]
fn the_first_refusal_in_body_order_is_the_one_named() {
    let body = "BEGIN
  NEW.txt := upper(NEW.txt);
  INSERT INTO audit (txt) VALUES (NEW.txt);
  NEW.h := md5(NEW::text);
  RETURN NEW;
EXCEPTION WHEN OTHERS THEN RETURN NEW;
END";
    assert_eq!(
        refusal_of_body(body),
        (
            "statement",
            texts(&["INSERT INTO audit (txt) VALUES (NEW.txt);"])
        )
    );
    let block = read(body);
    let refused = block.fields_of_new(&maintain()).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "the statement at line 3, column 3 neither assigns one field of NEW nor is an IF over \
         such assignments"
    );
    assert_eq!(
        refusal("NEW.a := 1; NEW.h := md5(NEW::text); RAISE 'late';"),
        ("read", texts(&["NEW", "NEW.h := md5(NEW::text);"]))
    );
}

#[test]
fn positions_count_in_the_body_as_written_around_an_integer_range() {
    let lines = "BEGIN NEW.a := 1;
  FOR i IN 1..10 LOOP NEW.b := i; END LOOP;
  RETURN NEW; END";
    assert_eq!(
        refusal_of_body(lines),
        (
            "statement",
            texts(&["FOR i IN 1..10 LOOP NEW.b := i; END LOOP;"])
        )
    );
    let comment = "BEGIN /* 1..2 */ NEW.a := 1; FOR i IN 1..2 LOOP NULL; END LOOP; RETURN NEW; END";
    assert_eq!(
        refusal_of_body(comment),
        ("statement", texts(&["FOR i IN 1..2 LOOP NULL; END LOOP;"]))
    );
}
