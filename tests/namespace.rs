//! Which declaration a name in a PL/pgSQL body reaches, as PostgreSQL 18
//! answers it, read through the public API.
#![cfg(feature = "body-parse")]

use std::{collections::HashSet, thread};

use sqlparser::{ast::Statement, dialect::PostgreSqlDialect, parser::Parser};
use sqlparser_plpgsql::{
    embedded::{self, Block, Dynamic, Error as ReadError},
    embedded_sql,
    namespace::{Error, FunctionKind, Name, NameKind, Signature, Site, Sql, TriggerVariable},
};

fn read(body: &str) -> Block {
    embedded_sql(body).unwrap_or_else(|error| panic!("{body}: {error}"))
}

fn signature(name: &str, parameters: &[Option<&str>], kind: FunctionKind) -> Signature {
    Signature {
        name: name.into(),
        parameters: parameters
            .iter()
            .map(|parameter| parameter.map(String::from))
            .collect(),
        kind,
    }
}

fn plain(name: &str, parameters: &[Option<&str>]) -> Signature {
    signature(name, parameters, FunctionKind::Plain)
}

/// Every site of `block`, every variable taken for a scalar.
fn sites<'body>(block: &'body Block, signature: &Signature) -> Vec<Site<'body>> {
    block
        .sites(signature, |_| false)
        .unwrap_or_else(|error| panic!("{error}"))
}

/// The sites of `block` that run SQL built at run time.
fn dynamic_sites<'body>(block: &'body Block, signature: &Signature) -> Vec<Site<'body>> {
    let mut sites = sites(block, signature);
    sites.retain(|site| matches!(site.sql, Sql::Dynamic(_)));
    sites
}

/// The SQL built at run time that `site` runs.
fn dynamic<'body>(site: &Site<'body>) -> &'body Dynamic {
    match site.sql {
        Sql::Dynamic(dynamic) => dynamic,
        other => panic!("expected SQL built at run time, found {other:?}"),
    }
}

/// What each site runs, one line each.
fn outline(sites: &[Site<'_>]) -> Vec<String> {
    sites
        .iter()
        .map(|site| match site.sql {
            Sql::Statement(statement) => format!("statement {statement}"),
            Sql::Expression(query) => format!("expression {query}"),
            Sql::Dynamic(dynamic) => format!("dynamic {}", dynamic.command),
        })
        .collect()
}

/// The variable `parts` reaches at `site`, every variable taken for a scalar,
/// with the number of parts it consumed.
fn reach<'site>(site: &'site Site<'_>, parts: &[&str]) -> Option<(&'site Name, usize)> {
    site.namespace.lookup(parts, |_| false)
}

/// The variable `parts` reaches whole at `site`.
fn whole<'site>(site: &'site Site<'_>, parts: &[&str]) -> &'site Name {
    match reach(site, parts) {
        Some((name, consumed)) if consumed == parts.len() => name,
        other => panic!("{parts:?} reaches {other:?}"),
    }
}

/// The name a declared variable is declared under.
fn declared_name(name: &Name) -> &str {
    match name.kind() {
        NameKind::Declared { declaration, .. } => &declaration.name,
        other => panic!("expected a declared variable, found {other:?}"),
    }
}

#[test]
fn every_site_comes_in_body_order_through_every_construct() {
    let block = read(
        "DECLARE r record; c refcursor;
        BEGIN
          EXECUTE 'one';
          PERFORM 1;
          BEGIN EXECUTE 'two'; END;
          IF a THEN EXECUTE 'three'; ELSIF b THEN EXECUTE 'four'; ELSE EXECUTE 'five'; END IF;
          CASE WHEN a THEN EXECUTE 'six'; ELSE EXECUTE 'seven'; END CASE;
          FOR r IN EXECUTE 'eight' LOOP EXECUTE 'nine'; END LOOP;
          WHILE a LOOP RETURN QUERY EXECUTE 'ten'; END LOOP;
          OPEN c FOR EXECUTE 'eleven';
          FOR i IN 1..2 LOOP EXECUTE 'twelve'; END LOOP;
        EXCEPTION
          WHEN division_by_zero THEN EXECUTE 'thirteen';
          WHEN others THEN EXECUTE 'fourteen';
        END",
    );
    let commands: Vec<String> = dynamic_sites(&block, &plain("f", &[]))
        .iter()
        .map(|site| dynamic(site).command.to_string())
        .collect();
    assert_eq!(
        commands,
        [
            "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
            "eleven", "twelve", "thirteen", "fourteen",
        ]
        .map(|text| format!("SELECT '{text}'"))
    );
}

#[test]
fn a_local_hides_a_parameter_that_the_function_label_and_position_still_reach() {
    let block = read("DECLARE p integer; BEGIN EXECUTE 'x'; END");
    let sites = dynamic_sites(&block, &plain("f", &[Some("p"), None]));
    let site = &sites[0];

    assert_eq!(declared_name(whole(site, &["p"])), "p");
    let qualified = reach(site, &["f", "p"]).expect("f.p reaches the parameter");
    assert_eq!(qualified.0.kind(), &NameKind::Parameter { position: 1 });
    assert_eq!(qualified.1, 2);
    let positional = reach(site, &["$1"]).expect("$1 reaches the parameter");
    assert_eq!(positional, (qualified.0, 1));
    assert_eq!(
        whole(site, &["$2"]).kind(),
        &NameKind::Parameter { position: 2 }
    );
    assert_eq!(reach(site, &["f", "$3"]), None);
}

#[test]
fn a_returned_column_is_reached_by_name_by_label_and_by_position() {
    let block = read("BEGIN EXECUTE 'x'; END");
    let sites = dynamic_sites(&block, &plain("tabled", &[Some("p"), Some("total")]));
    let site = &sites[0];
    let total = whole(site, &["total"]);
    assert_eq!(total.kind(), &NameKind::Parameter { position: 2 });
    assert_eq!(whole(site, &["tabled", "total"]), total);
    assert_eq!(whole(site, &["$2"]), total);
}

#[test]
fn an_inner_declaration_hides_an_outer_one_which_the_outer_label_still_reaches() {
    let block = read(
        "<<outer>> DECLARE x text;
        BEGIN DECLARE x integer; BEGIN EXECUTE 'x'; END; END",
    );
    let sites = dynamic_sites(&block, &plain("f", &[]));
    let inner = whole(&sites[0], &["x"]);
    let outer = whole(&sites[0], &["outer", "x"]);
    assert_ne!(inner, outer);
    assert_eq!(inner.id(), outer.id() + 1);
}

#[test]
fn a_qualified_name_passes_over_a_scalar_and_stops_at_a_row() {
    let block = read(
        "<<blk>> DECLARE x text;
        BEGIN DECLARE blk integer; BEGIN EXECUTE 'SELECT ' || blk.x; END; END",
    );
    let sites = dynamic_sites(&block, &plain("f", &[]));
    let site = &sites[0];
    let outer_x = whole(site, &["blk", "x"]);
    assert_eq!(declared_name(outer_x), "x");

    let inner_blk = whole(site, &["blk"]);
    let as_row = site
        .namespace
        .lookup(&["blk", "x"], |name| name == inner_blk);
    assert_eq!(as_row, Some((inner_blk, 1)));
}

#[test]
fn a_parameter_is_passed_over_as_a_scalar_and_read_from_as_a_row() {
    let block = read("BEGIN EXECUTE f.x; END");
    let sites = dynamic_sites(&block, &plain("f", &[Some("f"), Some("x")]));
    let namespace = &sites[0].namespace;
    let first = NameKind::Parameter { position: 1 };
    let second = NameKind::Parameter { position: 2 };
    let (scalar, consumed) = namespace.lookup(&["f", "x"], |_| false).unwrap();
    assert_eq!((scalar.kind(), consumed), (&second, 2));
    let (row, consumed) = namespace
        .lookup(&["f", "x"], |name| name.kind() == &first)
        .unwrap();
    assert_eq!((row.kind(), consumed), (&first, 1));
}

#[test]
fn a_third_part_passes_over_a_scalar_the_label_qualifies() {
    let block = read(
        "<<blk>> DECLARE x record;
        BEGIN EXECUTE 'outer'; DECLARE blk integer;
        BEGIN <<blk>> DECLARE x integer; BEGIN EXECUTE 'inner'; END; END; END",
    );
    let sites = dynamic_sites(&block, &plain("f", &[]));
    let outer_x = whole(&sites[0], &["x"]);
    let site = &sites[1];
    let inner_x = whole(site, &["blk", "x"]);
    assert_ne!(outer_x, inner_x);

    let row = |name: &Name| name == outer_x;
    assert_eq!(
        site.namespace.lookup(&["blk", "x", "y"], row),
        Some((outer_x, 2))
    );
    assert_eq!(site.namespace.lookup(&["blk", "x", "y", "z"], row), None);
    assert_eq!(site.namespace.lookup::<&str>(&[], row), None);
}

#[test]
fn a_loop_declares_its_variable_for_its_statements_only() {
    let block = read(
        "DECLARE i text; r record; c CURSOR FOR SELECT 1 AS v;
        BEGIN
          <<lp>> FOR i IN 1..2 LOOP EXECUTE 'in range'; END LOOP;
          EXECUTE 'after range';
          FOR r IN c LOOP EXECUTE 'in cursor'; END LOOP;
        END",
    );
    let sites = dynamic_sites(&block, &plain("f", &[]));
    let index = whole(&sites[0], &["i"]);
    assert_eq!(
        index.kind(),
        &NameKind::LoopIndex {
            variable: "i".into()
        }
    );
    assert_eq!(whole(&sites[0], &["lp", "i"]), index);
    assert_eq!(declared_name(whole(&sites[1], &["i"])), "i");
    match whole(&sites[2], &["r"]).kind() {
        NameKind::LoopRecord { variable, query } => {
            assert_eq!(variable, "r");
            assert_eq!(query.to_string(), "SELECT 1 AS v");
        }
        other => panic!("expected the cursor's record, found {other:?}"),
    }
    assert_eq!(declared_name(whole(&sites[1], &["r"])), "r");
}

#[test]
fn an_alias_of_a_loop_variable_names_the_variable_the_loop_declares() {
    let block = read(
        r#"DECLARE c CURSOR FOR SELECT 1 AS v;
        BEGIN
          FOR "Idx" IN 1..2 LOOP
            DECLARE a ALIAS FOR "Idx";
            BEGIN EXECUTE 'SELECT ' || a; END;
          END LOOP;
          FOR Rec IN c LOOP
            DECLARE b ALIAS FOR rec;
            BEGIN EXECUTE 'SELECT ' || b.v; END;
          END LOOP;
        END"#,
    );
    let sites = dynamic_sites(&block, &plain("f", &[]));
    assert_eq!(
        whole(&sites[0], &["a"]).kind(),
        &NameKind::LoopIndex {
            variable: "Idx".into()
        }
    );
    match whole(&sites[1], &["b"]).kind() {
        NameKind::LoopRecord { variable, .. } => assert_eq!(variable, "rec"),
        other => panic!("expected the cursor's record, found {other:?}"),
    }
}

#[test]
fn a_namespace_outlives_its_body_and_crosses_threads() {
    let namespace = {
        let block = read("DECLARE x text; BEGIN EXECUTE x; END");
        dynamic_sites(&block, &plain("f", &[Some("p")]))
            .remove(0)
            .namespace
    };
    let found = thread::spawn(move || {
        let (name, _) = namespace.lookup(&["x"], |_| false).unwrap();
        declared_name(name).to_owned()
    });
    assert_eq!(found.join().unwrap(), "x");
}

#[test]
fn an_exception_section_declares_sqlstate_and_sqlerrm_once_in_its_block() {
    let block = read(
        "<<blk>> BEGIN
          EXECUTE 'body';
        EXCEPTION
          WHEN division_by_zero THEN EXECUTE 'first';
          WHEN others THEN EXECUTE 'second';
        END",
    );
    let sites = dynamic_sites(&block, &plain("f", &[]));
    assert_eq!(reach(&sites[0], &["sqlstate"]), None);
    let sqlstate = whole(&sites[1], &["blk", "sqlstate"]);
    assert_eq!(sqlstate.kind(), &NameKind::SqlState);
    assert_eq!(whole(&sites[2], &["sqlstate"]), sqlstate);
    assert_eq!(whole(&sites[1], &["sqlerrm"]).kind(), &NameKind::SqlErrm);
}

#[test]
fn a_row_the_block_declares_comes_before_its_label_in_a_handler() {
    let block = read(
        "<<blk>> DECLARE blk record;
        BEGIN NULL; EXCEPTION WHEN others THEN EXECUTE 'x'; END",
    );
    let sites = dynamic_sites(&block, &plain("f", &[]));
    let row = whole(&sites[0], &["blk"]);
    assert_eq!(
        sites[0]
            .namespace
            .lookup(&["blk", "sqlstate"], |name| name == row),
        Some((row, 1))
    );
}

#[test]
fn found_is_everywhere_and_the_function_label_reaches_it() {
    let block = read("BEGIN EXECUTE 'x'; END");
    let sites = dynamic_sites(&block, &plain("f", &[]));
    let found = whole(&sites[0], &["found"]);
    assert_eq!(found.kind(), &NameKind::Found);
    assert_eq!(whole(&sites[0], &["f", "found"]), found);
    assert_eq!(reach(&sites[0], &["tg_op"]), None);
    assert_eq!(reach(&sites[0], &["$0"]), None);
}

#[test]
fn a_trigger_function_holds_its_trigger_variables() {
    let block = read("BEGIN EXECUTE 'x'; END");
    let sites = dynamic_sites(&block, &signature("t", &[], FunctionKind::Trigger));
    for variable in [
        TriggerVariable::New,
        TriggerVariable::Old,
        TriggerVariable::TgName,
        TriggerVariable::TgWhen,
        TriggerVariable::TgLevel,
        TriggerVariable::TgOp,
        TriggerVariable::TgRelid,
        TriggerVariable::TgRelname,
        TriggerVariable::TgTableName,
        TriggerVariable::TgTableSchema,
        TriggerVariable::TgNargs,
        TriggerVariable::TgArgv,
    ] {
        assert_eq!(
            whole(&sites[0], &[variable.name()]).kind(),
            &NameKind::Trigger(variable)
        );
    }
    assert_eq!(reach(&sites[0], &["tg_event"]), None);
    let new = whole(&sites[0], &["new"]);
    assert_eq!(reach(&sites[0], &["new", "id"]), Some((new, 1)));
    assert_eq!(reach(&sites[0], &["tg_op", "id"]), None);

    let sites = dynamic_sites(&block, &signature("e", &[], FunctionKind::EventTrigger));
    for variable in [TriggerVariable::TgEvent, TriggerVariable::TgTag] {
        assert_eq!(
            whole(&sites[0], &[variable.name()]).kind(),
            &NameKind::Trigger(variable)
        );
    }
    assert_eq!(reach(&sites[0], &["new"]), None);
}

#[test]
fn a_polymorphic_result_is_reached_only_as_dollar_zero() {
    let block = read("BEGIN EXECUTE 'x'; END");
    let sites = dynamic_sites(
        &block,
        &signature("poly", &[None], FunctionKind::PolymorphicResult),
    );
    let result = whole(&sites[0], &["$0"]);
    assert_eq!(result.kind(), &NameKind::ReturnValue);
    assert_eq!(result.id(), 1);
    assert_eq!(whole(&sites[0], &["found"]).id(), 2);
}

#[test]
fn an_alias_reaches_the_very_variable_it_names() {
    let block = read(
        "DECLARE a ALIAS FOR $2; b ALIAS FOR p; r record; s ALIAS FOR r.f; o ALIAS FOR al.p;
        BEGIN EXECUTE 'x'; END",
    );
    let signature = plain("al", &[Some("p"), None]);
    let sites = block
        .sites(&signature, |name| {
            matches!(name.kind(), NameKind::Declared { declaration, .. } if declaration.name == "r")
        })
        .expect("every alias names a variable");
    let site = &sites[0];
    assert_eq!(whole(site, &["a"]), whole(site, &["$2"]));
    assert_eq!(whole(site, &["b"]), whole(site, &["p"]));
    assert_eq!(whole(site, &["o"]), whole(site, &["p"]));
    assert_eq!(whole(site, &["s"]), whole(site, &["r"]));
}

#[test]
fn an_alias_for_nothing_is_an_error_naming_both() {
    let block = read("DECLARE c ALIAS FOR nothing; BEGIN NULL; END");
    let error = block
        .sites(&plain("f", &[]), |_| false)
        .expect_err("PostgreSQL refuses the body");
    assert_eq!(
        error,
        Error::UnknownAliasTarget {
            alias: "c".into(),
            target: "nothing".into(),
        }
    );
    assert!(error.to_string().contains("nothing"));

    let block = read("DECLARE c ALIAS FOR $2; BEGIN NULL; END");
    assert!(block.sites(&plain("f", &[None]), |_| false).is_err());
}

#[test]
fn a_type_copied_from_a_variable_reports_the_variable_where_it_is_declared() {
    let block = read(
        "DECLARE b p%TYPE; p integer; c p%TYPE; d f.p%TYPE; e t.name%TYPE; r record; g r.f%TYPE;
        BEGIN EXECUTE 'x'; END",
    );
    let signature = plain("f", &[Some("p")]);
    let sites = block
        .sites(&signature, |name| {
            matches!(name.kind(), NameKind::Declared { declaration, .. } if declaration.name == "r")
        })
        .expect("no alias to resolve");
    let site = &sites[0];
    let type_of = |name: &str| match whole(site, &[name]).kind() {
        NameKind::Declared { type_of, .. } => type_of.as_deref().map(Name::kind),
        other => panic!("{name} is {other:?}"),
    };
    assert_eq!(type_of("b"), Some(&NameKind::Parameter { position: 1 }));
    assert_eq!(type_of("c"), Some(whole(site, &["p"]).kind()));
    assert_eq!(type_of("d"), Some(&NameKind::Parameter { position: 1 }));
    assert_eq!(type_of("e"), None);
    assert_eq!(type_of("g"), None);
    assert_eq!(type_of("r"), None);
}

#[test]
fn every_variable_has_one_id_in_declaration_order() {
    let block = read(
        "DECLARE x int; y ALIAS FOR x;
        BEGIN
          FOR i IN 1..2 LOOP NULL; END LOOP;
          EXECUTE 'x';
        EXCEPTION WHEN others THEN EXECUTE 'y';
        END",
    );
    let sites = dynamic_sites(&block, &plain("f", &[Some("p"), None]));
    let ids = |site: &Site<'_>, names: &[&str]| -> Vec<usize> {
        names.iter().map(|name| whole(site, &[name]).id()).collect()
    };
    assert_eq!(
        ids(&sites[0], &["p", "$1", "$2", "found", "x", "y"]),
        [0, 0, 1, 2, 3, 3]
    );
    assert_eq!(ids(&sites[1], &["sqlstate", "sqlerrm"]), [5, 6]);
    assert_eq!(whole(&sites[0], &["x"]), whole(&sites[1], &["x"]));
    let parameter: HashSet<&Name> = [["p"].as_slice(), &["$1"], &["f", "p"]]
        .iter()
        .map(|parts| whole(&sites[0], parts))
        .collect();
    assert_eq!(parameter.len(), 1);
}

#[test]
fn a_command_is_the_expression_its_select_evaluates() {
    let block = read(
        "BEGIN
          EXECUTE c FROM commands() AS c;
          EXECUTE 'SELECT ' || t;
          EXECUTE 'plain';
          EXECUTE E'esc\\'aped';
          EXECUTE $q$dollar 'quoted'$q$;
          EXECUTE NULL;
          EXECUTE 'kept' ORDER BY 1;
          EXECUTE 'kept' WHERE false;
          EXECUTE 'kept' AS named;
        END",
    );
    let sites = dynamic_sites(&block, &plain("f", &[]));
    let expressions: Vec<String> = sites
        .iter()
        .map(|site| embedded::expression(&dynamic(site).command).to_string())
        .collect();
    assert_eq!(
        expressions,
        [
            "(SELECT c FROM commands() AS c)",
            "'SELECT ' || t",
            "'plain'",
            "E'esc\\'aped'",
            "$q$dollar 'quoted'$q$",
            "NULL",
            "(SELECT 'kept' ORDER BY 1)",
            "(SELECT 'kept' WHERE false)",
            "(SELECT 'kept' AS named)",
        ]
    );
    let constants: Vec<Option<&str>> = sites
        .iter()
        .map(|site| dynamic(site).constant_command())
        .collect();
    assert_eq!(
        constants,
        [
            None,
            None,
            Some("plain"),
            Some("esc'aped"),
            Some("dollar 'quoted'"),
            None,
            None,
            None,
            None,
        ]
    );
}

#[test]
fn a_set_operation_is_read_as_a_scalar_subquery() {
    let statement = Parser::parse_sql(&PostgreSqlDialect {}, "SELECT 1 UNION SELECT 2")
        .unwrap()
        .remove(0);
    let Statement::Query(query) = statement else {
        panic!("{statement}");
    };
    assert_eq!(
        embedded::expression(&query).to_string(),
        "(SELECT 1 UNION SELECT 2)"
    );
}

#[test]
fn a_second_declaration_of_a_name_in_one_section_is_refused() {
    for body in [
        "DECLARE x text; x integer; BEGIN NULL; END",
        "DECLARE x text; DECLARE x ALIAS FOR $1; BEGIN NULL; END",
        "DECLARE x text; x CURSOR FOR SELECT 1; BEGIN NULL; END",
    ] {
        assert!(
            matches!(embedded_sql(body), Err(ReadError::DuplicateDeclaration { ref name }) if name == "x"),
            "{body}: {:?}",
            embedded_sql(body)
        );
    }
    assert!(embedded_sql("DECLARE x text; BEGIN DECLARE x integer; BEGIN NULL; END; END").is_ok());
}

#[test]
fn every_statement_and_expression_comes_in_body_order() {
    let block = read(
        "DECLARE a int := 1; b int; c CURSOR (k int) FOR SELECT k; r record;
        BEGIN
          PERFORM 2;
          b := 3;
          IF 4 THEN RETURN 5; ELSIF 6 THEN NULL; ELSE RETURN 7; END IF;
          CASE 8 WHEN 9 THEN RETURN 10; ELSE RETURN 11; END CASE;
          WHILE 12 LOOP RETURN 13; END LOOP;
          FOR i IN 14 .. 15 BY 16 LOOP RETURN 17; END LOOP;
          FOR r IN SELECT 18 LOOP RETURN 19; END LOOP;
          FOR r IN c(20) LOOP RETURN 21; END LOOP;
          FOR r IN EXECUTE 'twenty-two' USING 23 LOOP RETURN 24; END LOOP;
          FOREACH b IN ARRAY 25 LOOP RETURN 26; END LOOP;
          OPEN c(27);
          DECLARE d int := 28; BEGIN RETURN 29; END;
        EXCEPTION WHEN others THEN RETURN 30;
        END",
    );
    assert_eq!(
        outline(&sites(&block, &plain("f", &[]))),
        [
            "expression SELECT 1",
            "statement SELECT 2",
            "statement SELECT 3",
            "expression SELECT 4",
            "statement SELECT 5",
            "expression SELECT 6",
            "statement SELECT 7",
            "expression SELECT 8",
            "expression SELECT 9",
            "statement SELECT 10",
            "statement SELECT 11",
            "expression SELECT 12",
            "statement SELECT 13",
            "expression SELECT 14",
            "expression SELECT 15",
            "expression SELECT 16",
            "statement SELECT 17",
            "statement SELECT 18",
            "statement SELECT 19",
            "expression SELECT 20",
            "statement SELECT k",
            "statement SELECT 21",
            "dynamic SELECT 'twenty-two'",
            "statement SELECT 24",
            "expression SELECT 25",
            "statement SELECT 26",
            "expression SELECT 27",
            "statement SELECT k",
            "expression SELECT 28",
            "statement SELECT 29",
            "statement SELECT 30",
        ]
    );
}

#[test]
fn a_name_in_any_statement_reaches_its_variable() {
    let block = read(
        "DECLARE label text := 'x';
        BEGIN
          PERFORM query_to_xml('SELECT ' || label || ' FROM items', false, false, '');
          RETURN query_to_xml('SELECT ' || p, false, false, '');
        END",
    );
    let sites = sites(&block, &plain("f", &[Some("p")]));
    assert_eq!(sites.len(), 3);
    assert_eq!(reach(&sites[0], &["label"]), None);
    assert_eq!(declared_name(whole(&sites[1], &["label"])), "label");
    assert_eq!(
        whole(&sites[2], &["p"]).kind(),
        &NameKind::Parameter { position: 1 }
    );
}

#[test]
fn a_default_reads_only_the_names_declared_before_it() {
    let block = read(
        "<<outer>> DECLARE y int := 1;
        BEGIN
          DECLARE x int := y; y text := x; z int := z;
          BEGIN RETURN y; END;
        END",
    );
    let sites = sites(&block, &plain("f", &[]));
    assert_eq!(reach(&sites[0], &["y"]), None);
    let outer_y = whole(&sites[1], &["y"]);
    assert_eq!(outer_y, whole(&sites[4], &["outer", "y"]));
    assert_eq!(reach(&sites[1], &["x"]), None);
    assert_eq!(declared_name(whole(&sites[2], &["x"])), "x");
    assert_eq!(whole(&sites[2], &["y"]), outer_y);
    assert_eq!(reach(&sites[3], &["z"]), None);
    let inner_y = whole(&sites[4], &["y"]);
    assert_eq!(declared_name(inner_y), "y");
    assert_ne!(inner_y, outer_y);
}

#[test]
fn a_loop_control_reads_the_names_outside_the_variable_the_loop_declares() {
    let block = read(
        "DECLARE i text; r text; c CURSOR (k int) FOR SELECT k;
        BEGIN
          FOR i IN length(i) .. 2 BY length(i) LOOP RETURN i; END LOOP;
          FOR r IN c(length(r)) LOOP RETURN r; END LOOP;
        END",
    );
    let sites = sites(&block, &plain("f", &[]));
    assert_eq!(declared_name(whole(&sites[0], &["i"])), "i");
    assert_eq!(declared_name(whole(&sites[2], &["i"])), "i");
    assert!(matches!(
        whole(&sites[3], &["i"]).kind(),
        NameKind::LoopIndex { .. }
    ));
    assert_eq!(declared_name(whole(&sites[4], &["r"])), "r");
    assert_eq!(declared_name(whole(&sites[5], &["r"])), "r");
    assert!(matches!(
        whole(&sites[6], &["r"]).kind(),
        NameKind::LoopRecord { .. }
    ));
}

#[test]
fn a_cursor_query_reads_its_parameters_and_the_names_where_it_is_declared() {
    let block = read(
        "<<outer>> DECLARE k text; c CURSOR (k int, m k%TYPE) FOR SELECT k, c.m, outer.k; later int;
        BEGIN
          DECLARE k int; d ALIAS FOR c; BEGIN OPEN d(1, 2); END;
          FOR r IN c(3, 4) LOOP NULL; END LOOP;
        END",
    );
    let sites = sites(&block, &plain("f", &[]));
    assert_eq!(outline(&sites)[2], "statement SELECT k, c.m, outer.k");
    let inner_k = whole(&sites[0], &["k"]);
    assert_eq!(inner_k.id(), 6);

    let query = &sites[2];
    let parameter = whole(query, &["k"]);
    match parameter.kind() {
        NameKind::CursorParameter { parameter, type_of } => {
            assert_eq!(parameter.name, "k");
            assert_eq!(type_of, &None);
        }
        other => panic!("expected a cursor parameter, found {other:?}"),
    }
    assert_eq!(whole(query, &["c", "k"]), parameter);
    match whole(query, &["c", "m"]).kind() {
        NameKind::CursorParameter {
            parameter: m,
            type_of,
        } => {
            assert_eq!(m.name, "m");
            assert_eq!(type_of.as_deref(), Some(parameter));
        }
        other => panic!("expected a cursor parameter, found {other:?}"),
    }
    let outer_k = whole(query, &["outer", "k"]);
    assert_eq!(declared_name(outer_k), "k");
    assert_eq!(
        [outer_k.id(), parameter.id(), whole(query, &["m"]).id()],
        [1, 2, 3]
    );
    assert_eq!(reach(query, &["c"]), None);
    assert_eq!(reach(query, &["later"]), None);

    assert_eq!(declared_name(whole(&sites[3], &["k"])), "k");
    assert_eq!(whole(&sites[5], &["k"]), parameter);
}

#[test]
fn opening_what_a_row_answers_for_instead_of_a_cursor_is_an_error() {
    let block =
        read("<<r>> DECLARE c CURSOR FOR SELECT 1; r record; a ALIAS FOR r.c; BEGIN OPEN a; END");
    assert!(block.sites(&plain("f", &[]), |_| false).is_ok());
    let error = block
        .sites(&plain("f", &[]), |name| {
            matches!(name.kind(), NameKind::Declared { declaration, .. } if declaration.name == "r")
        })
        .expect_err("a is the record, which PostgreSQL does not open");
    assert_eq!(error, Error::UnboundCursor { name: "a".into() });
}

#[test]
fn a_body_declaring_many_names_is_read_and_dropped_without_exhausting_the_stack() {
    let declarations: Vec<String> = (0..20_000).map(|index| format!("v{index} int;")).collect();
    let block = read(&format!(
        "DECLARE {} BEGIN RETURN v0; END",
        declarations.join(" ")
    ));
    let sites = sites(&block, &plain("f", &[]));
    assert_eq!(whole(&sites[0], &["v0"]).id(), 1);
}
