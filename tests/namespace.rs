//! Which declaration a name in a PL/pgSQL body reaches, as PostgreSQL 18
//! answers it, read through the public API.
#![cfg(feature = "body-parse")]

use std::{collections::HashSet, thread};

use sqlparser::{ast::Statement, dialect::PostgreSqlDialect, parser::Parser};
use sqlparser_plpgsql::{
    embedded::{self, Block, Dynamic, Error as ReadError, LoopControl, Node, NodeKind},
    embedded_sql,
    namespace::{
        Error, FunctionKind, Name, NameKind, Namespace, Signature, Site, Sql, TriggerVariable,
        Write, WriteSite,
    },
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
          CLOSE c;
          COMMIT;
          ROLLBACK AND CHAIN;
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
        NameKind::LoopRecord {
            variable, query, ..
        } => {
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
          LOOP EXIT WHEN 30; EXIT; END LOOP;
          RAISE 'thirty-one %', 32 USING HINT = 33;
          ASSERT 34, 35;
          ASSERT 36;
          RETURN;
        EXCEPTION WHEN others THEN RETURN 37;
        END",
    );
    assert_eq!(
        outline(&sites(&block, &plain("f", &[]))),
        [
            "expression SELECT 1",
            "statement SELECT 2",
            "expression SELECT 3",
            "expression SELECT 4",
            "expression SELECT 5",
            "expression SELECT 6",
            "expression SELECT 7",
            "expression SELECT 8",
            "expression SELECT 9",
            "expression SELECT 10",
            "expression SELECT 11",
            "expression SELECT 12",
            "expression SELECT 13",
            "expression SELECT 14",
            "expression SELECT 15",
            "expression SELECT 16",
            "expression SELECT 17",
            "statement SELECT 18",
            "expression SELECT 19",
            "expression SELECT 20",
            "statement SELECT k",
            "expression SELECT 21",
            "dynamic SELECT 'twenty-two'",
            "expression SELECT 24",
            "expression SELECT 25",
            "expression SELECT 26",
            "expression SELECT 27",
            "statement SELECT k",
            "expression SELECT 28",
            "expression SELECT 29",
            "expression SELECT 30",
            "expression SELECT 32",
            "expression SELECT 33",
            "expression SELECT 34",
            "expression SELECT 35",
            "expression SELECT 36",
            "expression SELECT 37",
        ]
    );
}

#[test]
fn rows_a_body_returns_or_discards_are_read_as_postgresql_compiles_them() {
    let block = read(
        "BEGIN
          PERFORM 1;
          RETURN NEXT 2;
          RETURN QUERY SELECT 3;
          RETURN QUERY EXECUTE 'four';
        END",
    );
    assert_eq!(
        outline(&sites(&block, &plain("f", &[]))),
        [
            "statement SELECT 1",
            "expression SELECT 2",
            "statement SELECT 3",
            "dynamic SELECT 'four'",
        ]
    );
}

#[test]
fn an_assignment_reads_its_subscripts_then_its_value() {
    let block = read(
        "DECLARE a int[]; p pair[]; i int;
        BEGIN a[i] := 1; a[2:i] := 3; a[:4] := i; p[i].x[2] := 5; END",
    );
    let sites = sites(&block, &plain("f", &[]));
    assert_eq!(
        outline(&sites),
        [
            "expression SELECT i",
            "expression SELECT 1",
            "expression SELECT 2",
            "expression SELECT i",
            "expression SELECT 3",
            "expression SELECT 4",
            "expression SELECT i",
            "expression SELECT i",
            "expression SELECT 2",
            "expression SELECT 5",
        ]
    );
    let i = whole(&sites[0], &["i"]);
    assert_eq!(declared_name(i), "i");
    assert_eq!(whole(&sites[6], &["i"]), i);
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
fn a_body_declaring_many_names_is_read_shown_and_dropped_without_exhausting_the_stack() {
    let declarations: Vec<String> = (0..20_000).map(|index| format!("v{index} int;")).collect();
    let block = read(&format!(
        "DECLARE {} BEGIN RETURN v0; END",
        declarations.join(" ")
    ));
    let sites = sites(&block, &plain("f", &[]));
    assert_eq!(whole(&sites[0], &["v0"]).id(), 1);
    let shown = format!("{:?}", sites[0].namespace);
    let latest = shown.find("\"v19999\"").expect("the latest name is shown");
    assert!(latest < shown.find("\"v0\"").expect("the first name is shown"));
    assert!(shown.contains("\"found\""));
}

/// The write sites of `block`, every variable taken for a scalar.
fn writes<'body>(block: &'body Block, signature: &Signature) -> Vec<WriteSite<'body>> {
    block
        .write_sites(signature, |_| false)
        .unwrap_or_else(|error| panic!("{error}"))
}

/// The variable `parts` reaches whole in `namespace`.
fn whole_in<'namespace>(namespace: &'namespace Namespace, parts: &[&str]) -> &'namespace Name {
    match namespace.lookup(parts, |_| false) {
        Some((name, consumed)) if consumed == parts.len() => name,
        other => panic!("{parts:?} reaches {other:?}"),
    }
}

/// The node a write site borrows.
fn node<'body>(site: &WriteSite<'body>) -> &'body Node {
    match site.write {
        Write::Node(node) => node,
        Write::Declaration(declaration) => panic!("expected a node, found {declaration:?}"),
    }
}

#[test]
fn fetch_and_move_yield_direction_expressions_in_body_order() {
    let block = read(
        "BEGIN
          PERFORM 0;
          FETCH ABSOLUTE position FROM cur INTO target;
          MOVE FORWARD amount FROM cur;
          FETCH RELATIVE distance IN cur INTO target;
          MOVE BACKWARD remaining IN cur;
          MOVE ABSOLUTE ending FROM cur;
          MOVE RELATIVE offset_value FROM cur;
          MOVE 2 FROM cur;
          FETCH FIRST FROM cur INTO target;
          MOVE ALL FROM cur;
          MOVE BACKWARD ALL FROM cur;
          MOVE NEXT FROM cur;
          PERFORM 9;
        END",
    );
    let signature = plain("f", &[Some("position"), Some("amount"), Some("distance")]);
    let sites = sites(&block, &signature);
    assert_eq!(
        outline(&sites),
        [
            "statement SELECT 0",
            "expression SELECT position",
            "expression SELECT amount",
            "expression SELECT distance",
            "expression SELECT remaining",
            "expression SELECT ending",
            "expression SELECT offset_value",
            "expression SELECT 2",
            "statement SELECT 9",
        ]
    );
    for (index, name) in [(1, "position"), (2, "amount"), (3, "distance")] {
        assert_eq!(
            whole(&sites[index], &[name]).kind(),
            &NameKind::Parameter { position: index }
        );
    }
    let writes = writes(&block, &signature);
    assert_eq!(writes.len(), 3);
    for (site, index) in writes.iter().zip([1, 3, 8]) {
        assert!(core::ptr::eq(
            node(site),
            &raw const block.statements[index]
        ));
    }
}

#[test]
fn every_write_comes_in_body_order_borrowing_its_node() {
    let block = read(
        "DECLARE s text := 'a'; t text; r record; arr text[]; c CURSOR (k int) FOR SELECT k; cur refcursor; y ALIAS FOR s;
        BEGIN
          s := 'b';
          PERFORM 1;
          SELECT 'c' INTO t;
          UPDATE docs SET v = 1;
          CALL p(t);
          EXECUTE 'SELECT 1' INTO STRICT t;
          EXECUTE 'SELECT 2';
          OPEN cur FOR EXECUTE 'SELECT 3';
          FETCH cur INTO t;
          GET DIAGNOSTICS t = ROW_COUNT;
          FOR r IN SELECT 1 LOOP NULL; END LOOP;
          FOR r IN EXECUTE 'SELECT 4' LOOP NULL; END LOOP;
          FOR i IN 1..2 LOOP NULL; END LOOP;
          FOR r IN c(1) LOOP NULL; END LOOP;
          FOREACH t IN ARRAY arr LOOP NULL; END LOOP;
          WHILE false LOOP NULL; END LOOP;
          OPEN c(2);
        END",
    );
    let writes = writes(&block, &plain("f", &[]));
    let kinds: Vec<&str> = writes
        .iter()
        .map(|site| match site.write {
            Write::Declaration(declaration) => declaration.name.as_str(),
            Write::Node(node) => match &node.kind {
                NodeKind::Assignment(_) => "assignment",
                NodeKind::Statement { statement, .. } => match **statement {
                    Statement::Call(_) => "call",
                    _ => "statement",
                },
                NodeKind::Dynamic { .. } => "execute",
                NodeKind::Open(_) => "open",
                NodeKind::Fetch(_) => "fetch",
                NodeKind::Diagnostics(_) => "diagnostics",
                NodeKind::Loop(looped) => match looped.control {
                    LoopControl::Query { .. } => "query loop",
                    LoopControl::Dynamic { .. } => "dynamic loop",
                    LoopControl::Range { .. } => "range loop",
                    LoopControl::Cursor { .. } => "cursor loop",
                    LoopControl::Array { .. } => "array loop",
                    LoopControl::Unconditional | LoopControl::While(_) => "plain loop",
                },
                other => panic!("{other:?} writes nothing"),
            },
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "s",
            "t",
            "r",
            "arr",
            "c",
            "cur",
            "assignment",
            "statement",
            "call",
            "execute",
            "open",
            "fetch",
            "diagnostics",
            "query loop",
            "dynamic loop",
            "range loop",
            "cursor loop",
            "array loop",
            "open",
        ]
    );
    for site in &writes[6..] {
        assert!(
            block
                .statements
                .iter()
                .any(|statement| core::ptr::eq(statement, node(site))),
            "{site:?} borrows a node of the block"
        );
    }
}

#[test]
fn a_write_in_an_inner_block_reaches_the_variable_it_shadows_or_not() {
    let block = read(
        "DECLARE s text;
        BEGIN
          s := 'outer';
          DECLARE s text;
          BEGIN s := 'inner'; END;
          s := 'again';
        END",
    );
    let writes = writes(&block, &plain("f", &[]));
    let assigned: Vec<&Name> = writes
        .iter()
        .filter(|site| matches!(site.write, Write::Node(Node { kind: NodeKind::Assignment(assignment), .. }) if assignment.target.name.to_string() == "s"))
        .map(|site| whole_in(&site.namespace, &["s"]))
        .collect();
    let [outer, inner, again] = assigned.as_slice() else {
        panic!("expected three writes of s, found {assigned:?}");
    };
    assert_ne!(outer.id(), inner.id());
    assert_eq!(outer.id(), again.id());
    let declared: Vec<usize> = writes
        .iter()
        .filter_map(|site| site.declared.as_ref().map(Name::id))
        .collect();
    assert_eq!(declared, [outer.id(), inner.id()]);
}

#[test]
fn a_query_loop_assigns_the_variables_in_reach_and_declares_none() {
    let block = read(
        "<<blk>> DECLARE r record; a int; b int;
        BEGIN
          <<lp>> FOR r IN SELECT 1 AS v LOOP EXECUTE 'SELECT ' || r.v; END LOOP;
          FOR blk.a, b IN EXECUTE 'SELECT 1, 2' LOOP NULL; END LOOP;
        END",
    );
    let signature = plain("f", &[]);
    let writes = writes(&block, &signature);
    let declared_r = writes[0].declared.as_ref().expect("r is declared");
    let query_loop = &writes[3];
    let NodeKind::Loop(looped) = &node(query_loop).kind else {
        panic!("expected a loop");
    };
    let LoopControl::Query { targets, query } = &looped.control else {
        panic!("expected a query loop");
    };
    assert_eq!(query.to_string(), "SELECT 1 AS v");
    assert_eq!(targets.len(), 1);
    assert_eq!(query_loop.declared, None);
    assert_eq!(whole_in(&query_loop.namespace, &["r"]), declared_r);
    assert_eq!(whole_in(&query_loop.namespace, &["blk", "r"]), declared_r);
    // The loop's statements read the same variable, not a record the loop declares.
    let execute = &dynamic_sites(&block, &signature)[0];
    assert_eq!(
        dynamic(execute).command.to_string(),
        "SELECT 'SELECT ' || r.v"
    );
    assert_eq!(whole(execute, &["r"]), declared_r);

    let dynamic_loop = &writes[4];
    let NodeKind::Loop(looped) = &node(dynamic_loop).kind else {
        panic!("expected a loop");
    };
    let LoopControl::Dynamic { targets, .. } = &looped.control else {
        panic!("expected a dynamic loop");
    };
    let ids: Vec<usize> = targets
        .iter()
        .map(|target| {
            let parts: Vec<String> = target.name.0.iter().map(ToString::to_string).collect();
            let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
            whole_in(&dynamic_loop.namespace, &parts).id()
        })
        .collect();
    let declared_ids: Vec<usize> = writes[1..3]
        .iter()
        .map(|site| site.declared.as_ref().map(Name::id).unwrap())
        .collect();
    assert_eq!(ids, declared_ids);
}

#[test]
fn a_cursor_loop_and_an_open_write_where_their_arguments_are_read() {
    let block = read(
        "DECLARE k int := 1; c CURSOR (k int) FOR SELECT c.k + k AS v; r record;
        BEGIN
          FOR r IN c(5) LOOP EXECUTE 'SELECT ' || r.v; END LOOP;
          OPEN c(k);
        END",
    );
    let signature = plain("f", &[]);
    let writes = writes(&block, &signature);
    let body_k = writes[0].declared.as_ref().expect("k is declared");
    let cursor = writes[1].declared.as_ref().expect("c is declared");
    let cursor_loop = &writes[3];
    let Some(record) = &cursor_loop.declared else {
        panic!("a cursor loop declares its record");
    };
    let NameKind::LoopRecord { query, .. } = record.kind() else {
        panic!("expected the loop's record, found {record:?}");
    };
    assert_eq!(query.to_string(), "SELECT c.k + k AS v");
    assert_eq!(whole_in(&cursor_loop.namespace, &["k"]), body_k);
    let [execute] = dynamic_sites(&block, &signature).try_into().unwrap();
    assert_eq!(whole(&execute, &["r"]), record);
    let open = &writes[4];
    let NodeKind::Open(opened) = &node(open).kind else {
        panic!("expected an OPEN");
    };
    assert_eq!(opened.cursor().to_string(), "c");
    assert_eq!(whole_in(&open.namespace, &["k"]), body_k);
    assert_eq!(whole_in(&open.namespace, &["c"]), cursor);
}

#[test]
fn an_open_reads_its_query_in_its_block_and_writes_its_cursor() {
    let block = read(
        "DECLARE s text; cur refcursor;
        BEGIN
          DECLARE s text;
          BEGIN OPEN cur FOR EXECUTE 'SELECT ' || s USING s; OPEN cur FOR SELECT s; END;
        END",
    );
    let signature = plain("f", &[]);
    let [site] = dynamic_sites(&block, &signature).try_into().unwrap();
    assert_eq!(dynamic(&site).command.to_string(), "SELECT 'SELECT ' || s");
    assert_eq!(dynamic(&site).arguments.len(), 1);
    let writes = writes(&block, &signature);
    let inner_s = writes[2]
        .declared
        .as_ref()
        .expect("the inner s is declared");
    assert_eq!(whole(&site, &["s"]).id(), inner_s.id());
    let sites = sites(&block, &signature);
    assert_eq!(
        outline(&sites),
        ["dynamic SELECT 'SELECT ' || s", "statement SELECT s"]
    );
    assert_eq!(whole(&sites[1], &["s"]), inner_s);
    for open in &writes[3..] {
        let NodeKind::Open(opened) = &node(open).kind else {
            panic!("expected an OPEN");
        };
        assert_eq!(opened.cursor().to_string(), "cur");
        assert_eq!(whole_in(&open.namespace, &["s"]), inner_s);
    }
    assert_eq!(writes.len(), 5);
}

#[test]
fn a_declaration_and_a_range_loop_read_their_values_without_their_own_variable() {
    let block = read(
        "DECLARE s text := 'a'; i int := 2;
        BEGIN
          DECLARE s text := s;
          BEGIN FOR i IN 1..i LOOP NULL; END LOOP; END;
        END",
    );
    let writes = writes(&block, &plain("f", &[]));
    let outer_s = writes[0].declared.as_ref().unwrap();
    let outer_i = writes[1].declared.as_ref().unwrap();
    let inner = &writes[2];
    assert_eq!(whole_in(&inner.namespace, &["s"]), outer_s);
    assert_ne!(inner.declared.as_ref(), Some(outer_s));
    let range = &writes[3];
    assert_eq!(whole_in(&range.namespace, &["i"]), outer_i);
    assert!(matches!(
        range.declared.as_ref().map(Name::kind),
        Some(NameKind::LoopIndex { variable }) if variable == "i"
    ));
}

#[test]
fn a_declaration_reaches_the_names_its_block_declares_before_it() {
    let block = read("DECLARE a int := 1; b int := a; BEGIN NULL; END");
    let writes = writes(&block, &plain("f", &[]));
    let a = writes[0].declared.as_ref().unwrap();
    let b = writes[1].declared.as_ref().unwrap();
    let reach = |site: &WriteSite<'_>, name: &str| {
        site.namespace
            .lookup(&[name], |_| false)
            .map(|(found, _)| found.clone())
    };
    assert_eq!(reach(&writes[0], "a"), None);
    assert_eq!(reach(&writes[0], "b"), None);
    assert_eq!(reach(&writes[1], "a").as_ref(), Some(a));
    assert_eq!(reach(&writes[1], "b"), None);
    assert_ne!(a, b);
}
