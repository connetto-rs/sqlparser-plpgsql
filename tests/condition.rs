//! The SQLSTATE a PL/pgSQL condition stands for, as PostgreSQL 18 resolves
//! it, read through the public API.

use sqlparser_plpgsql::condition::{ConditionName, UnknownCondition, errcode};
use sqlstate::{SqlState, pg};

fn code(text: &str) -> SqlState {
    text.parse().expect("an SQLSTATE")
}

#[test]
fn a_name_listed_under_two_codes_raises_the_first_and_is_caught_under_both() {
    for (name, codes) in [
        ("null_value_not_allowed", ["22004", "39004"]),
        ("modifying_sql_data_not_permitted", ["2F002", "38002"]),
        ("prohibited_sql_statement_attempted", ["2F003", "38003"]),
        ("reading_sql_data_not_permitted", ["2F004", "38004"]),
    ] {
        let condition: ConditionName = name.parse().expect("a condition name");
        assert_eq!(condition.as_str(), name);
        assert_eq!(condition.sqlstate(), code(codes[0]), "{name}");
        assert_eq!(
            condition.sqlstates().collect::<Vec<_>>(),
            codes.map(code),
            "{name}"
        );
    }
}

#[test]
fn a_name_resolves_to_the_code_postgresql_lists_for_it() {
    for (name, expected) in [
        ("check_violation", pg::CHECK_VIOLATION),
        ("raise_exception", pg::RAISE_EXCEPTION),
        ("assert_failure", pg::ASSERT_FAILURE),
        (
            "integrity_constraint_violation",
            pg::INTEGRITY_CONSTRAINT_VIOLATION,
        ),
        ("idle_session_timeout", pg::IDLE_SESSION_TIMEOUT),
    ] {
        let condition: ConditionName = name.parse().expect("a condition name");
        assert_eq!(condition.sqlstate(), expected, "{name}");
        assert_eq!(condition.sqlstates().collect::<Vec<_>>(), [expected]);
    }
}

#[test]
fn only_an_error_with_a_condition_name_is_a_condition() {
    for name in [
        "others",
        "Check_Violation",
        "successful_completion",
        "warning",
        "no_data",
        "23514",
        "",
    ] {
        assert_eq!(
            name.parse::<ConditionName>(),
            Err(UnknownCondition { name: name.into() })
        );
    }
}

#[test]
fn errcode_reads_a_code_before_a_name() {
    for (text, expected) in [
        ("P0001", Some(pg::RAISE_EXCEPTION)),
        ("23514", Some(pg::CHECK_VIOLATION)),
        ("ZZ999", Some(code("ZZ999"))),
        ("00000", None),
        ("check_violation", Some(pg::CHECK_VIOLATION)),
        ("null_value_not_allowed", Some(code("22004"))),
    ] {
        assert_eq!(errcode(text), Ok(expected), "{text}");
    }
}

#[test]
fn errcode_refuses_text_that_is_neither_a_code_nor_a_name() {
    for text in [
        "p0001",
        "P000",
        "P00001",
        "Check_Violation",
        "others",
        " 23514",
    ] {
        let error = errcode(text).expect_err(text);
        assert_eq!(
            error.to_string(),
            format!("unrecognized exception condition \"{text}\"")
        );
    }
}
