//! What a trigger body that only assigns fields of `NEW` leaves in each of
//! them, read through the public API.
#![cfg(feature = "body-parse")]

use sqlparser_plpgsql::{
    embedded_sql,
    namespace::{FunctionKind, Signature},
};

/// The fields `body`, the body of the trigger function `maintain()`, leaves
/// in `NEW`, each written as its field and the expression it holds.
fn fields_of_body(body: &str) -> Option<Vec<(String, String)>> {
    let block = embedded_sql(body).unwrap_or_else(|error| panic!("{body}: {error}"));
    let signature = Signature {
        name: "maintain".into(),
        parameters: Vec::new(),
        kind: FunctionKind::Trigger,
    };
    block.fields_of_new(&signature).map(|fields| {
        fields
            .into_iter()
            .map(|assigned| (assigned.field.to_string(), assigned.value.to_string()))
            .collect()
    })
}

/// The fields `statements` leave in `NEW` when `RETURN NEW` follows them.
fn fields(statements: &str) -> Option<Vec<(String, String)>> {
    fields_of_body(&format!("BEGIN {statements} RETURN NEW; END"))
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|&(field, value)| (field.into(), value.into()))
        .collect()
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
        Some(pairs(&[(
            "archived_at",
            "CASE WHEN NEW.archived_at IS NOT NULL THEN CASE WHEN TG_OP = 'UPDATE' AND \
             OLD.archived_at IS NOT NULL THEN OLD.archived_at ELSE now() END ELSE \
             NEW.archived_at END",
        )]))
    );
}

#[test]
fn an_elsif_chain_without_else_keeps_the_value_before_the_if() {
    assert_eq!(
        fields("IF NEW.a < 0 THEN NEW.a := 0; ELSIF NEW.a > 10 THEN NEW.a := 10; END IF;"),
        Some(pairs(&[(
            "a",
            "CASE WHEN NEW.a < 0 THEN 0 WHEN NEW.a > 10 THEN 10 ELSE NEW.a END"
        )]))
    );
}

#[test]
fn fields_assigned_in_different_branches_each_get_a_case() {
    assert_eq!(
        fields(
            "IF TG_OP = 'INSERT' THEN NEW.created_at := now(); \
             ELSE NEW.updated_at := now(); END IF;"
        ),
        Some(pairs(&[
            (
                "created_at",
                "CASE WHEN TG_OP = 'INSERT' THEN now() ELSE NEW.created_at END"
            ),
            (
                "updated_at",
                "CASE WHEN TG_OP = 'INSERT' THEN NEW.updated_at ELSE now() END"
            ),
        ]))
    );
}

#[test]
fn a_read_after_an_assignment_reads_the_assigned_value() {
    assert_eq!(
        fields("NEW.a := NEW.a + 1; NEW.b := NEW.a * 2;"),
        Some(pairs(&[("a", "NEW.a + 1"), ("b", "(NEW.a + 1) * 2")]))
    );
    assert_eq!(
        fields("NEW.a := 1; IF NEW.a > 0 THEN NEW.b := NEW.a; END IF;"),
        Some(pairs(&[
            ("a", "1"),
            ("b", "CASE WHEN 1 > 0 THEN 1 ELSE NEW.b END")
        ]))
    );
    assert_eq!(
        fields("IF TG_OP = 'INSERT' THEN NEW.a := 1; END IF; NEW.a := NEW.a + 1;"),
        Some(pairs(&[(
            "a",
            "CASE WHEN TG_OP = 'INSERT' THEN 1 ELSE NEW.a END + 1"
        )]))
    );
    assert_eq!(
        fields("NEW.a := 1; NEW.b := (SELECT x FROM t WHERE t.id = NEW.a);"),
        Some(pairs(&[
            ("a", "1"),
            ("b", "(SELECT x FROM t WHERE t.id = 1)")
        ]))
    );
    assert_eq!(
        fields("NEW.a := 1; NEW.b := CURRENT_TIMESTAMP;"),
        Some(pairs(&[("a", "1"), ("b", "CURRENT_TIMESTAMP")]))
    );
}

#[test]
fn a_field_is_the_name_postgresql_stores_and_keeps_its_first_spelling() {
    assert_eq!(
        fields("NEW.Total := 1; NEW.total := NEW.TOTAL + 1;"),
        Some(pairs(&[("Total", "1 + 1")]))
    );
    assert_eq!(
        fields("NEW.\"ColA\" := 1; NEW.b := NEW.\"ColA\"; NEW.c := NEW.cola;"),
        Some(pairs(&[("\"ColA\"", "1"), ("b", "1"), ("c", "NEW.cola")]))
    );
}

#[test]
fn new_qualified_by_the_function_name_is_new() {
    assert_eq!(
        fields_of_body(
            "BEGIN maintain.NEW.a := 1; NEW.b := maintain.new.a + 1; RETURN maintain.NEW; END"
        ),
        Some(pairs(&[("a", "1"), ("b", "1 + 1")]))
    );
    assert_eq!(
        fields("NEW.a := 1; NEW.h := md5(maintain.NEW::text);"),
        None
    );
}

#[test]
fn the_whole_row_is_read_only_before_any_assignment() {
    for read in [
        "md5(NEW::text)",
        "md5(ROW(NEW.*)::text)",
        "md5(row_to_json(NEW.*)::text)",
        "md5((0, NEW.*)::text)",
        "(SELECT md5(r::text) FROM (SELECT NEW.*) AS r)",
    ] {
        let before = fields(&format!("NEW.h := {read}; NEW.a := 1;"))
            .unwrap_or_else(|| panic!("{read} reads NEW before any assignment"));
        assert_eq!(before[1], ("a".into(), "1".into()), "{read}");
        assert_eq!(
            fields(&format!("NEW.a := 1; NEW.h := {read};")),
            None,
            "{read}"
        );
    }
    assert_eq!(
        fields("NEW.h := md5(NEW::text); NEW.a := 1;"),
        Some(pairs(&[("h", "md5(NEW::TEXT)"), ("a", "1")]))
    );
    let expands_a = "NEW.b := ROW(NEW.a.*); NEW.a := ROW(1);";
    assert!(fields(expands_a).is_some(), "{expands_a}");
    assert_eq!(fields(&format!("{expands_a} NEW.c := ROW(NEW.a.*);")), None);
    let reads_a_field = "NEW.b := NEW.a.f1; NEW.a := ROW(1);";
    assert!(fields(reads_a_field).is_some(), "{reads_a_field}");
    assert_eq!(fields(&format!("{reads_a_field} NEW.c := NEW.a.f1;")), None);
}

#[test]
fn any_other_statement_refuses_the_body() {
    for statement in [
        "RAISE NOTICE 'stamping';",
        "PERFORM pg_notify('t', 'stamped');",
        "INSERT INTO t (a) VALUES (1);",
        "LOOP EXIT; END LOOP;",
        "IF NEW.a IS NULL THEN RETURN NEW; END IF;",
        "BEGIN NEW.a := 2; END;",
        "NEW.a[1] := 2;",
        "NEW.a.b := 2;",
    ] {
        assert_eq!(
            fields(&format!("NEW.a := 1; {statement}")),
            None,
            "{statement}"
        );
    }
}

#[test]
fn a_body_that_does_not_return_new_or_assigns_elsewhere_is_refused() {
    for body in [
        "BEGIN NEW.a := 1; RETURN NULL; END",
        "BEGIN NEW.a := 1; END",
        "BEGIN RETURN NEW; END",
        "DECLARE x int; BEGIN NEW.a := 1; RETURN NEW; END",
        "BEGIN NEW.a := 1; RETURN NEW; EXCEPTION WHEN OTHERS THEN RETURN NEW; END",
        "BEGIN OLD.a := 1; RETURN NEW; END",
    ] {
        assert_eq!(fields_of_body(body), None, "{body}");
    }
}
