//! The SQL a PL/pgSQL body embeds, read through the public API.
#![cfg(feature = "body-parse")]

mod common;

use core::fmt::Write as _;

use common::quoted;
use sqlparser::ast::{FunctionArguments, ObjectName, Spanned, Statement};
use sqlparser_plpgsql::{
    embedded::{
        AliasTarget, Block, BoundCursor, Branch, Condition, DeclarationKind, DeclaredType, Dynamic,
        Error, ExitKind, Fetch, FetchCount, FetchDirection, FetchPosition, Indirection,
        IntoTargets, Loop, LoopControl, Node, NodeKind, Open, Raise, ReturnQuery, Scrolling,
        Target,
    },
    embedded_sql,
};
use sqlstate::{SqlState, pg};

/// One line per node, indented by depth, so a test states the whole tree.
fn outline(body: &str) -> String {
    let block = embedded_sql(body).unwrap_or_else(|error| panic!("{body}: {error}"));
    let mut out = String::new();
    write_block(&mut out, &block, 0);
    out
}

fn line(out: &mut String, depth: usize, text: &str) {
    writeln!(out, "{}{text}", "  ".repeat(depth)).expect("writing to a string");
}

fn write_block(out: &mut String, block: &Block, depth: usize) {
    line(out, depth, &labelled(block.label.as_deref(), "block"));
    for declaration in &block.declarations {
        line(
            out,
            depth + 1,
            &format!(
                "declare {} {}",
                declaration.name,
                describe(&declaration.kind)
            ),
        );
    }
    write_nodes(out, &block.statements, depth + 1);
    for handler in &block.handlers {
        let conditions: Vec<String> = handler.conditions.iter().map(condition_text).collect();
        line(
            out,
            depth + 1,
            &format!("handler {}", conditions.join(" or ")),
        );
        write_nodes(out, &handler.statements, depth + 2);
    }
}

fn condition_text(condition: &Condition) -> String {
    match condition {
        Condition::Name(name) => format!("condition {name}"),
        Condition::Sqlstate(code) => format!("sqlstate {code}"),
        Condition::Others => "others".into(),
    }
}

fn labelled(label: Option<&str>, text: &str) -> String {
    match label {
        Some(label) => format!("<<{label}>> {text}"),
        None => text.into(),
    }
}

fn describe(kind: &DeclarationKind) -> String {
    match kind {
        DeclarationKind::Variable(variable) => {
            let mut text = String::new();
            if variable.constant {
                text.push_str("constant ");
            }
            text.push_str(&type_text(&variable.data_type));
            if let Some(collation) = &variable.collation {
                write!(text, " collate {collation}").expect("writing to a string");
            }
            if variable.not_null {
                text.push_str(" not null");
            }
            if let Some(default) = &variable.default {
                write!(text, " default {default}").expect("writing to a string");
            }
            text
        }
        DeclarationKind::Alias(AliasTarget::Parameter(position)) => {
            format!("alias for ${position}")
        }
        DeclarationKind::Alias(AliasTarget::Name(name)) => format!("alias for {name}"),
        DeclarationKind::Cursor(cursor) if cursor.parameters.is_empty() => {
            format!("{}cursor", scrolling_text(cursor.scrolling))
        }
        DeclarationKind::Cursor(cursor) => {
            let parameters: Vec<String> = cursor
                .parameters
                .iter()
                .map(|parameter| format!("{} {}", parameter.name, type_text(&parameter.data_type)))
                .collect();
            format!(
                "{}cursor ({})",
                scrolling_text(cursor.scrolling),
                parameters.join(", ")
            )
        }
    }
}

fn type_text(data_type: &DeclaredType) -> String {
    let brackets = |array: bool| if array { "[]" } else { "" };
    match data_type {
        DeclaredType::Data(data_type) => data_type.to_string(),
        DeclaredType::ColumnType { of, array } => format!("{of}%TYPE{}", brackets(*array)),
        DeclaredType::RowType { of, array } => format!("{of}%ROWTYPE{}", brackets(*array)),
    }
}

fn scrolling_text(scrolling: Scrolling) -> &'static str {
    match scrolling {
        Scrolling::Unspecified => "",
        Scrolling::Scroll => "scroll ",
        Scrolling::NoScroll => "no scroll ",
    }
}

fn write_dynamic(out: &mut String, dynamic: &Dynamic, depth: usize) {
    line(out, depth, &format!("dynamic {}", dynamic.command));
    for argument in &dynamic.arguments {
        line(out, depth + 1, &format!("using {argument}"));
    }
}

/// The cursor `opened` opens, then its arguments a level deeper.
fn write_bound_cursor(out: &mut String, opened: &str, cursor: &BoundCursor, depth: usize) {
    line(
        out,
        depth,
        &format!(
            "{opened} {}{}: {}",
            scrolling_text(cursor.scrolling),
            cursor.name,
            cursor.query
        ),
    );
    for argument in &cursor.arguments {
        line(out, depth + 1, &format!("argument {argument}"));
    }
}

/// A target as written, each subscript as the `SELECT` evaluating it.
fn target_text(target: &Target) -> String {
    let mut text = target.name.to_string();
    for step in &target.indirection {
        match step {
            Indirection::Field(field) => write!(text, ".{field}"),
            Indirection::Index(index) => write!(text, "[{index}]"),
            Indirection::Slice { lower, upper } => {
                let bound = |bound: &Option<Box<_>>| {
                    bound.as_ref().map(ToString::to_string).unwrap_or_default()
                };
                write!(text, "[{}:{}]", bound(lower), bound(upper))
            }
        }
        .expect("writing to a string");
    }
    text
}

fn targets_text(targets: &[Target]) -> String {
    targets
        .iter()
        .map(target_text)
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_into(out: &mut String, into: Option<&IntoTargets>, depth: usize) {
    if let Some(into) = into {
        let strict = if into.strict { "strict " } else { "" };
        line(
            out,
            depth,
            &format!("into {strict}{}", targets_text(&into.targets)),
        );
    }
}

fn write_branches(out: &mut String, branches: &[Branch], otherwise: Option<&[Node]>, depth: usize) {
    for branch in branches {
        line(out, depth, &format!("when {}", branch.condition));
        write_nodes(out, &branch.statements, depth + 1);
    }
    if let Some(otherwise) = otherwise {
        line(out, depth, "else");
        write_nodes(out, otherwise, depth + 1);
    }
}

fn write_nodes(out: &mut String, nodes: &[Node], depth: usize) {
    for node in nodes {
        match &node.kind {
            NodeKind::Statement { statement, into } => {
                line(out, depth, &statement.to_string());
                write_into(out, into.as_ref(), depth + 1);
            }
            NodeKind::Perform { query } => line(out, depth, &format!("perform {query}")),
            NodeKind::Assignment(assignment) => line(
                out,
                depth,
                &format!(
                    "{} := {}",
                    target_text(&assignment.target),
                    assignment.value
                ),
            ),
            NodeKind::Dynamic { dynamic, into } => {
                write_dynamic(out, dynamic, depth);
                write_into(out, into.as_ref(), depth + 1);
            }
            NodeKind::Fetch(fetch) => write_fetch(out, fetch, depth),
            NodeKind::Move { cursor, direction } => write_move(out, cursor, direction, depth),
            NodeKind::Diagnostics(diagnostics) => {
                let stacked = if diagnostics.stacked { " stacked" } else { "" };
                line(out, depth, &format!("diagnostics{stacked}"));
                for item in &diagnostics.items {
                    let text = format!("{} = {:?}", target_text(&item.target), item.kind);
                    line(out, depth + 1, &text);
                }
            }
            NodeKind::Open(open) => write_open(out, open, depth),
            NodeKind::Close { cursor } => line(out, depth, &format!("close {cursor}")),
            NodeKind::Commit { chain } => write_transaction(out, "commit", *chain, depth),
            NodeKind::Rollback { chain } => write_transaction(out, "rollback", *chain, depth),
            NodeKind::Block(block) => write_block(out, block, depth),
            NodeKind::If(conditional) => {
                line(out, depth, "if");
                write_branches(
                    out,
                    &conditional.branches,
                    (!conditional.otherwise.is_empty()).then_some(&conditional.otherwise),
                    depth + 1,
                );
            }
            NodeKind::Case(case) => {
                match &case.subject {
                    Some(subject) => line(out, depth, &format!("case {subject}")),
                    None => line(out, depth, "case"),
                }
                write_branches(out, &case.branches, case.otherwise.as_deref(), depth + 1);
            }
            NodeKind::Loop(looped) => {
                write_loop_control(out, looped, depth);
                write_nodes(out, &looped.statements, depth + 1);
            }
            NodeKind::Exit(exit) => {
                let mut text = String::from(match exit.kind {
                    ExitKind::Exit => "exit",
                    ExitKind::Continue => "continue",
                });
                if let Some(label) = &exit.label {
                    write!(text, " {label}").expect("writing to a string");
                }
                if let Some(condition) = &exit.condition {
                    write!(text, " when {condition}").expect("writing to a string");
                }
                line(out, depth, &text);
            }
            NodeKind::Return { value: None } => line(out, depth, "return"),
            NodeKind::Return { value: Some(value) } => line(out, depth, &format!("return {value}")),
            NodeKind::ReturnNext { value: None } => line(out, depth, "return next"),
            NodeKind::ReturnNext { value: Some(value) } => {
                line(out, depth, &format!("return next {value}"));
            }
            NodeKind::ReturnQuery(ReturnQuery::Query(query)) => {
                line(out, depth, &format!("return query {query}"));
            }
            NodeKind::ReturnQuery(ReturnQuery::Dynamic(dynamic)) => {
                line(out, depth, "return query");
                write_dynamic(out, dynamic, depth + 1);
            }
            NodeKind::Raise(raise) => write_raise(out, raise, depth),
            NodeKind::Assert(assert) => {
                line(out, depth, &format!("assert {}", assert.condition));
                if let Some(message) = &assert.message {
                    line(out, depth + 1, &format!("message {message}"));
                }
            }
        }
    }
}

fn write_raise(out: &mut String, raise: &Raise, depth: usize) {
    let mut text = format!("raise {:?}", raise.level);
    if let Some(condition) = &raise.condition {
        write!(text, " {}", condition_text(condition)).expect("writing to a string");
    }
    if let Some(format) = &raise.format {
        write!(text, " format {format:?}").expect("writing to a string");
    }
    line(out, depth, &text);
    for argument in &raise.arguments {
        line(out, depth + 1, &format!("argument {argument}"));
    }
    for option in &raise.options {
        let text = format!("option {:?} = {}", option.kind, option.value);
        line(out, depth + 1, &text);
    }
}

fn write_move(out: &mut String, cursor: &ObjectName, direction: &FetchDirection, depth: usize) {
    line(
        out,
        depth,
        &format!("move {} {cursor}", direction_text(direction)),
    );
}

fn direction_text(direction: &FetchDirection) -> String {
    match direction {
        FetchDirection::Forward(count) => format!("forward {}", count_text(count)),
        FetchDirection::Backward(count) => format!("backward {}", count_text(count)),
        FetchDirection::Absolute(position) => match position {
            FetchPosition::First => String::from("first"),
            FetchPosition::Last => String::from("last"),
            FetchPosition::Row(query) => format!("absolute {query}"),
        },
        FetchDirection::Relative(query) => format!("relative {query}"),
    }
}

fn count_text(count: &FetchCount) -> String {
    match count {
        FetchCount::One => String::from("one"),
        FetchCount::All => String::from("all"),
        FetchCount::Rows(query) => query.to_string(),
    }
}

fn write_fetch(out: &mut String, fetch: &Fetch, depth: usize) {
    line(
        out,
        depth,
        &format!(
            "fetch {} {} into {}",
            direction_text(&fetch.direction),
            fetch.cursor,
            targets_text(&fetch.targets)
        ),
    );
}

fn write_transaction(out: &mut String, command: &str, chain: bool, depth: usize) {
    let suffix = if chain { " and chain" } else { "" };
    line(out, depth, &format!("{command}{suffix}"));
}

fn write_open(out: &mut String, open: &Open, depth: usize) {
    match open {
        Open::Bound(cursor) => write_bound_cursor(out, "open", cursor, depth),
        Open::Query {
            cursor,
            query,
            scrolling,
        } => {
            line(
                out,
                depth,
                &format!("open {cursor} {}for {query}", scrolling_text(*scrolling)),
            );
        }
        Open::Dynamic {
            cursor,
            dynamic,
            scrolling,
        } => {
            line(
                out,
                depth,
                &format!("open {cursor} {}for", scrolling_text(*scrolling)),
            );
            write_dynamic(out, dynamic, depth + 1);
        }
    }
}

fn write_loop_control(out: &mut String, looped: &Loop, depth: usize) {
    let label = looped.label.as_deref();
    match &looped.control {
        LoopControl::Unconditional => line(out, depth, &labelled(label, "loop")),
        LoopControl::While(condition) => {
            line(out, depth, &labelled(label, &format!("while {condition}")));
        }
        LoopControl::Range {
            variable,
            reverse,
            lower,
            upper,
            step,
        } => {
            let direction = if *reverse { " reverse" } else { "" };
            let text = format!("range {variable} in{direction} {lower} .. {upper}");
            line(out, depth, &labelled(label, &text));
            if let Some(step) = step {
                line(out, depth + 1, &format!("by {step}"));
            }
        }
        LoopControl::Query { targets, query } => {
            let text = format!("query {} in {query}", targets_text(targets));
            line(out, depth, &labelled(label, &text));
        }
        LoopControl::Cursor { variable, cursor } => {
            let opened = labelled(label, &format!("cursor {variable} in"));
            write_bound_cursor(out, &opened, cursor, depth);
        }
        LoopControl::Dynamic { targets, dynamic } => {
            let text = format!("loop over {}", targets_text(targets));
            line(out, depth, &labelled(label, &text));
            write_dynamic(out, dynamic, depth + 1);
        }
        LoopControl::Array {
            targets,
            slice,
            array,
        } => {
            let text = format!("array {} slice {slice} in {array}", targets_text(targets));
            line(out, depth, &labelled(label, &text));
        }
    }
}

/// Strips the common indentation of an expected outline written inline.
fn expected(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let indent = lines
        .iter()
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    lines.iter().fold(String::new(), |mut out, text| {
        line(&mut out, 0, &text[indent..]);
        out
    })
}

#[test]
fn every_statement_postgresql_ran_is_read_in_place() {
    let body = "
        DECLARE r RECORD; c CURSOR FOR SELECT id FROM pp_docs; n INT;
        BEGIN
          PERFORM 1 FROM pp_docs;
          FOR r IN SELECT id FROM pp_docs LOOP INSERT INTO pp_log VALUES (r.id); END LOOP;
          OPEN c; FETCH c INTO n; CLOSE c;
          BEGIN UPDATE pp_docs SET id = id; EXCEPTION WHEN others THEN DELETE FROM pp_log; END;
          RETURN QUERY SELECT id FROM pp_docs;
        END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare r RECORD
              declare c cursor
              declare n INT
              perform SELECT 1 FROM pp_docs
              query r in SELECT id FROM pp_docs
                INSERT INTO pp_log VALUES (r.id)
              open c: SELECT id FROM pp_docs
              fetch forward one c into n
              close c
              block
                UPDATE pp_docs SET id = id
                handler others
                  DELETE FROM pp_log
              return query SELECT id FROM pp_docs
            "
        )
    );
}

#[test]
fn close_and_transactions_keep_the_cursor_and_chain() {
    let body = r#"
        <<outer>> DECLARE c refcursor; "Quoted" refcursor;
        BEGIN
          NULL;
          CLOSE c;
          CLOSE outer.c;
          CLOSE "Quoted";
          COMMIT;
          COMMIT AND CHAIN;
          COMMIT AND NO CHAIN;
          ROLLBACK;
          ROLLBACK AND NO CHAIN;
          ROLLBACK AND CHAIN;
          NULL;
        END"#;
    assert_eq!(
        outline(body),
        expected(
            r#"
            <<outer>> block
              declare c refcursor
              declare Quoted refcursor
              close c
              close outer.c
              close "Quoted"
              commit
              commit and chain
              commit
              rollback
              rollback
              rollback and chain
            "#
        )
    );
}

#[test]
fn a_definer_update_and_its_return_value_are_read() {
    let body = "BEGIN
        UPDATE public.photos SET content_state = p_new_state WHERE content_id = p_file_id;
        RETURN p_file_id;
    END;";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              UPDATE public.photos SET content_state = p_new_state WHERE content_id = p_file_id
              return SELECT p_file_id
            "
        )
    );
}

#[test]
fn every_into_clause_is_kept_apart_from_its_statement() {
    let body = "DECLARE n INT; x INT; y INT; r RECORD;
    BEGIN
        SELECT count(*) INTO n FROM docs;
        SELECT INTO x, y a, b FROM docs;
        SELECT * INTO STRICT r FROM docs WHERE id = 1;
        INSERT INTO docs VALUES (1) RETURNING id INTO n;
        UPDATE docs SET v = 1 RETURNING id, v INTO STRICT x, \"Y\";
        DELETE FROM docs WHERE id = 1 RETURNING * INTO outer_block.r;
        MERGE INTO docs USING src ON docs.id = src.id WHEN MATCHED THEN DELETE;
        SELECT 1 FROM docs INTO r.field;
    END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare n INT
              declare x INT
              declare y INT
              declare r RECORD
              SELECT count(*) FROM docs
                into n
              SELECT a, b FROM docs
                into x, y
              SELECT * FROM docs WHERE id = 1
                into strict r
              INSERT INTO docs VALUES (1) RETURNING id
                into n
              UPDATE docs SET v = 1 RETURNING id, v
                into strict x, \"Y\"
              DELETE FROM docs WHERE id = 1 RETURNING *
                into outer_block.r
              MERGE INTO docs USING src ON docs.id = src.id WHEN MATCHED THEN DELETE
              SELECT 1 FROM docs
                into r.field
            "
        )
    );
}

#[test]
fn every_expression_is_read_as_the_select_postgresql_evaluates() {
    let body = "
        DECLARE
          total INT := (SELECT count(*) FROM docs);
          fallback INT DEFAULT 0;
          limit_value CONSTANT INT = 3;
          alias_value ALIAS FOR $1;
          r RECORD;
          arr INT[];
        DECLARE
          extra INT := 4;
        BEGIN
          total := a FROM docs WHERE id = 1;
          r.field = (SELECT max(id) FROM docs);
          arr[(SELECT 1 FROM docs)] := 2;
          \"total\" := 5;
          arr[1:2] := ARRAY[1, 2];
          arr[arr[1]] := 3;
          arr[:2] := ARRAY[5];
          arr[3:] := ARRAY[6];
          pairs[1].first.inner := 7;
          RAISE NOTICE 'count % of %', total, (SELECT 1 FROM logs) USING HINT = (SELECT 'h' FROM hints);
          ASSERT total > 0, (SELECT 'bad' FROM messages);
          ASSERT (SELECT true FROM checks);
          RETURN NEXT (SELECT 2 FROM docs);
          RETURN;
        END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare total INT default SELECT (SELECT count(*) FROM docs)
              declare fallback INT default SELECT 0
              declare limit_value constant INT default SELECT 3
              declare alias_value alias for $1
              declare r RECORD
              declare arr INT[]
              declare extra INT default SELECT 4
              total := SELECT a FROM docs WHERE id = 1
              r.field := SELECT (SELECT max(id) FROM docs)
              arr[SELECT (SELECT 1 FROM docs)] := SELECT 2
              \"total\" := SELECT 5
              arr[SELECT 1:SELECT 2] := SELECT ARRAY[1, 2]
              arr[SELECT arr[1]] := SELECT 3
              arr[:SELECT 2] := SELECT ARRAY[5]
              arr[SELECT 3:] := SELECT ARRAY[6]
              pairs[SELECT 1].first.inner := SELECT 7
              raise Notice format \"count % of %\"
                argument SELECT total
                argument SELECT (SELECT 1 FROM logs)
                option Hint = SELECT (SELECT 'h' FROM hints)
              assert SELECT total > 0
                message SELECT (SELECT 'bad' FROM messages)
              assert SELECT (SELECT true FROM checks)
              return next SELECT (SELECT 2 FROM docs)
              return
            "
        )
    );
}

#[test]
fn branches_keep_their_conditions_and_statements_apart() {
    let body = "BEGIN
        IF EXISTS (SELECT 1 FROM a) THEN
          SET search_path = app;
        ELSIF x = 'THEN' THEN
          DELETE FROM b;
        ELSEIF y THEN
        ELSE
          RESET search_path;
        END IF;
        CASE kind
          WHEN 1, 2 THEN UPDATE c SET v = 1;
          WHEN 3 THEN DELETE FROM e;
          ELSE DELETE FROM c;
        END CASE;
        CASE WHEN (SELECT true FROM d) THEN NULL; END CASE;
        CASE WHEN (SELECT false FROM d) THEN NULL; ELSE END CASE;
    END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              if
                when SELECT EXISTS (SELECT 1 FROM a)
                  SET search_path = app
                when SELECT x = 'THEN'
                  DELETE FROM b
                when SELECT y
                else
                  RESET search_path
              case SELECT kind
                when SELECT 1, 2
                  UPDATE c SET v = 1
                when SELECT 3
                  DELETE FROM e
                else
                  DELETE FROM c
              case
                when SELECT (SELECT true FROM d)
              case
                when SELECT (SELECT false FROM d)
                else
            "
        )
    );
}

#[test]
fn every_loop_keeps_its_control_apart_from_its_body() {
    let body = "
        DECLARE c CURSOR (key INT) FOR SELECT id FROM docs WHERE id = key; x INT[]; a INT; b INT; y INT;
          pair CURSOR (first_id INT, last_id INT) FOR SELECT id FROM docs WHERE id BETWEEN first_id AND last_id;
          plain CURSOR FOR SELECT id FROM plain_docs;
        BEGIN
          <<outer>>
          LOOP EXIT outer WHEN (SELECT true FROM stop); END LOOP outer;
          WHILE (SELECT count(*) FROM queue) > 0 LOOP DELETE FROM queue; END LOOP;
          FOR i IN 1..10 LOOP CONTINUE WHEN i = 2; END LOOP;
          FOR i IN REVERSE (SELECT max(id) FROM docs) .. lo BY 2 LOOP NULL; END LOOP;
          FOR r IN c(key := 7) LOOP NULL; END LOOP;
          FOR r IN c(v) LOOP NULL; END LOOP;
          FOR r IN pair(1, (SELECT 2 FROM bounds)) LOOP NULL; END LOOP;
          FOR r IN plain LOOP EXIT; END LOOP;
          FOREACH y IN ARRAY x LOOP NULL; END LOOP;
          FOREACH x SLICE 1 IN ARRAY (SELECT array_agg(id) FROM docs) LOOP NULL; END LOOP;
          FOREACH a, b IN ARRAY pairs LOOP NULL; END LOOP;
          FOR a, b IN WITH w AS (SELECT 1, 2) SELECT * FROM w LOOP NULL; END LOOP;
          FOR r.field IN SELECT 1 LOOP NULL; END LOOP;
        END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare c cursor (key INT)
              declare x INT[]
              declare a INT
              declare b INT
              declare y INT
              declare pair cursor (first_id INT, last_id INT)
              declare plain cursor
              <<outer>> loop
                exit outer when SELECT (SELECT true FROM stop)
              while SELECT (SELECT count(*) FROM queue) > 0
                DELETE FROM queue
              range i in SELECT 1 .. SELECT 10
                continue when SELECT i = 2
              range i in reverse SELECT (SELECT max(id) FROM docs) .. SELECT lo
                by SELECT 2
              cursor r in c: SELECT id FROM docs WHERE id = key
                argument SELECT 7
              cursor r in c: SELECT id FROM docs WHERE id = key
                argument SELECT v
              cursor r in pair: SELECT id FROM docs WHERE id BETWEEN first_id AND last_id
                argument SELECT 1
                argument SELECT (SELECT 2 FROM bounds)
              cursor r in plain: SELECT id FROM plain_docs
                exit
              array y slice 0 in SELECT x
              array x slice 1 in SELECT (SELECT array_agg(id) FROM docs)
              array a, b slice 0 in SELECT pairs
              query a, b in WITH w AS (SELECT 1, 2) SELECT * FROM w
              query r.field in SELECT 1
            "
        )
    );
}

#[test]
fn a_cursor_query_runs_where_the_cursor_opens() {
    let body = "
        DECLARE
          c CURSOR FOR SELECT id FROM outer_docs;
          never NO SCROLL CURSOR FOR SELECT id FROM unread;
          d SCROLL CURSOR (k INT) IS SELECT id FROM keyed WHERE id = k;
          free refcursor;
          n INT;
        BEGIN
          IF flag THEN OPEN c; END IF;
          DECLARE c CURSOR FOR SELECT id FROM inner_docs;
          BEGIN OPEN c; END;
          OPEN d(k => (SELECT 1 FROM keys));
          OPEN free SCROLL FOR SELECT id FROM loose;
          FETCH RELATIVE (SELECT 2 FROM steps) FROM d INTO n;
          MOVE FORWARD ALL IN d;
          FETCH NEXT FROM d INTO n;
          FETCH FROM d INTO n, r.field;
          FETCH d INTO n;
          OPEN c;
        END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare c cursor
              declare never no scroll cursor
              declare d scroll cursor (k INT)
              declare free refcursor
              declare n INT
              if
                when SELECT flag
                  open c: SELECT id FROM outer_docs
              block
                declare c cursor
                open c: SELECT id FROM inner_docs
              open scroll d: SELECT id FROM keyed WHERE id = k
                argument SELECT (SELECT 1 FROM keys)
              open free scroll for SELECT id FROM loose
              fetch relative SELECT (SELECT 2 FROM steps) d into n
              move forward all d
              fetch forward one d into n
              fetch forward one d into n, r.field
              fetch forward one d into n
              open c: SELECT id FROM outer_docs
            "
        )
    );
}

#[test]
fn fetch_and_move_keep_every_direction() {
    for (clause, direction) in [
        ("", "forward one"),
        ("FROM ", "forward one"),
        ("IN ", "forward one"),
        ("NEXT FROM ", "forward one"),
        ("PRIOR IN ", "backward one"),
        ("FIRST FROM ", "first"),
        ("LAST IN ", "last"),
        ("FORWARD FROM ", "forward one"),
        ("BACKWARD IN ", "backward one"),
        ("ABSOLUTE 3 FROM ", "absolute SELECT 3"),
        ("RELATIVE -2 IN ", "relative SELECT -2"),
        (
            "ABSOLUTE (SELECT position FROM positions) FROM ",
            "absolute SELECT (SELECT position FROM positions)",
        ),
    ] {
        assert_eq!(
            outline(&format!(
                "BEGIN FETCH {clause}c INTO r; MOVE {clause}c; END"
            )),
            expected(&format!(
                "block\n  fetch {direction} c into r\n  move {direction} c"
            ))
        );
    }
    for (clause, direction) in [
        ("ALL FROM ", "forward all"),
        ("FORWARD ALL IN ", "forward all"),
        ("BACKWARD ALL FROM ", "backward all"),
        ("FORWARD amount FROM ", "forward SELECT amount"),
        ("BACKWARD amount IN ", "backward SELECT amount"),
        ("2 FROM ", "forward SELECT 2"),
        (
            "(SELECT amount FROM steps) IN ",
            "forward SELECT (SELECT amount FROM steps)",
        ),
    ] {
        assert_eq!(
            outline(&format!("BEGIN MOVE {clause}c; END")),
            expected(&format!("block\n  move {direction} c"))
        );
    }
}

#[test]
fn cursor_scrolling_survives_declaration_open_and_iteration() {
    let body = "
        DECLARE
          forwards SCROLL CURSOR FOR SELECT 1;
          one_way NO SCROLL CURSOR FOR SELECT 1;
          automatic CURSOR FOR SELECT 1;
          free refcursor;
        BEGIN
          OPEN forwards;
          OPEN one_way;
          OPEN automatic;
          FOR r IN forwards LOOP NULL; END LOOP;
          FOR r IN one_way LOOP NULL; END LOOP;
          FOR r IN automatic LOOP NULL; END LOOP;
          OPEN free SCROLL FOR SELECT 1;
          OPEN free NO SCROLL FOR EXECUTE 'SELECT 1';
        END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare forwards scroll cursor
              declare one_way no scroll cursor
              declare automatic cursor
              declare free refcursor
              open scroll forwards: SELECT 1
              open no scroll one_way: SELECT 1
              open automatic: SELECT 1
              cursor r in scroll forwards: SELECT 1
              cursor r in no scroll one_way: SELECT 1
              cursor r in automatic: SELECT 1
              open free scroll for SELECT 1
              open free no scroll for
                dynamic SELECT 'SELECT 1'
            "
        )
    );
}

#[test]
fn sql_built_at_run_time_is_reported_with_the_expressions_that_build_it() {
    let body = "DECLARE n INT; cur refcursor; r RECORD;
    BEGIN
        EXECUTE 'DELETE FROM ' || quote_ident(t) USING a, (SELECT 1 FROM b) INTO n;
        EXECUTE format('SELECT %I', c) INTO STRICT n USING d;
        RETURN QUERY EXECUTE 'SELECT 1';
        OPEN cur NO SCROLL FOR EXECUTE 'SELECT 2' USING e;
        FOR r IN EXECUTE 'SELECT 3' USING f LOOP NULL; END LOOP;
    END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare n INT
              declare cur refcursor
              declare r RECORD
              dynamic SELECT 'DELETE FROM ' || quote_ident(t)
                using SELECT a
                using SELECT (SELECT 1 FROM b)
                into n
              dynamic SELECT format('SELECT %I', c)
                using SELECT d
                into strict n
              return query
                dynamic SELECT 'SELECT 1'
              open cur no scroll for
                dynamic SELECT 'SELECT 2'
                  using SELECT e
              loop over r
                dynamic SELECT 'SELECT 3'
                  using SELECT f
            "
        )
    );
}

#[test]
fn each_command_that_runs_a_query_keeps_what_it_does_with_the_rows() {
    let body = "DECLARE n INT;
    BEGIN
        PERFORM 1;
        SELECT 1;
        SELECT 1 INTO n;
        RETURN NEXT 1;
        RETURN NEXT;
        RETURN QUERY SELECT 1;
        RETURN QUERY EXECUTE 'SELECT 1';
        EXECUTE 'SELECT 1';
        RETURN 1;
    END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare n INT
              perform SELECT 1
              SELECT 1
              SELECT 1
                into n
              return next SELECT 1
              return next
              return query SELECT 1
              return query
                dynamic SELECT 'SELECT 1'
              dynamic SELECT 'SELECT 1'
              return SELECT 1
            "
        )
    );
}

#[test]
fn each_handler_belongs_to_the_block_that_owns_it() {
    let body = "
        <<top>>
        DECLARE n INT := 1; msg TEXT;
        BEGIN
          <<inner>>
          DECLARE m INT := (SELECT 2 FROM defaults);
          BEGIN
            SET search_path = a;
          EXCEPTION
            WHEN unique_violation OR SQLSTATE '22012' THEN INSERT INTO errors VALUES (1);
            WHEN others THEN
              BEGIN RAISE; EXCEPTION WHEN others THEN NULL; END;
          END inner;
          UPDATE docs SET v = 1;
        EXCEPTION WHEN others THEN
          GET STACKED DIAGNOSTICS msg = MESSAGE_TEXT;
          DELETE FROM docs;
        END top;";
    assert_eq!(
        outline(body),
        expected(
            "
            <<top>> block
              declare n INT default SELECT 1
              declare msg TEXT
              <<inner>> block
                declare m INT default SELECT (SELECT 2 FROM defaults)
                SET search_path = a
                handler condition unique_violation or sqlstate 22012
                  INSERT INTO errors VALUES (1)
                handler others
                  block
                    raise Exception
                    handler others
              UPDATE docs SET v = 1
              handler others
                diagnostics stacked
                  msg = MessageText
                DELETE FROM docs
            "
        )
    );
}

#[test]
fn each_handler_keeps_the_conditions_it_catches() {
    let body = r#"BEGIN
        NULL;
      EXCEPTION
        WHEN Insufficient_Privilege THEN RETURN 1;
        WHEN SQLSTATE '42501' OR "no_data_found" THEN RETURN 2;
        WHEN unique_violation OR division_by_zero OR sqlstate $$P0001$$ THEN RETURN 3;
        WHEN "sqlstate" '22012' THEN RETURN 4;
        WHEN OTHERS THEN RETURN 5;
    END"#;
    assert_eq!(
        outline(body),
        expected(
            "
            block
              handler condition insufficient_privilege
                return SELECT 1
              handler sqlstate 42501 or condition no_data_found
                return SELECT 2
              handler condition unique_violation or condition division_by_zero or sqlstate P0001
                return SELECT 3
              handler sqlstate 22012
                return SELECT 4
              handler others
                return SELECT 5
            "
        )
    );
}

#[test]
fn a_condition_resolves_to_the_sqlstate_postgresql_gives_it() {
    let block = embedded_sql(
        "BEGIN
          RAISE check_violation;
          RAISE null_value_not_allowed;
          RAISE SQLSTATE 'P1234';
          RAISE SQLSTATE '00000';
        EXCEPTION WHEN null_value_not_allowed OR others OR SQLSTATE '22012' THEN NULL;
        END",
    )
    .expect("a body PostgreSQL accepts");
    let raised: Vec<Option<SqlState>> = block
        .statements
        .iter()
        .map(|node| {
            let NodeKind::Raise(raise) = &node.kind else {
                panic!("{node:?} is no RAISE")
            };
            raise.condition.and_then(Condition::sqlstate)
        })
        .collect();
    assert_eq!(
        raised,
        [
            Some(pg::CHECK_VIOLATION),
            Some(pg::NULL_VALUE_NOT_ALLOWED),
            Some("P1234".parse().expect("an SQLSTATE")),
            None,
        ]
    );
    let [
        Condition::Name(name),
        Condition::Others,
        Condition::Sqlstate(code),
    ] = block.handlers[0].conditions[..]
    else {
        panic!("{:?}", block.handlers[0].conditions)
    };
    assert_eq!(
        name.sqlstates().collect::<Vec<_>>(),
        [
            pg::NULL_VALUE_NOT_ALLOWED,
            pg::E_R_I_E_NULL_VALUE_NOT_ALLOWED
        ]
    );
    assert_eq!(code, pg::DIVISION_BY_ZERO);
    assert_eq!(Condition::Others.sqlstate(), None);
}

#[test]
fn a_condition_postgresql_does_not_know_is_refused() {
    for (body, name) in [
        ("BEGIN RAISE no_such_condition; END", "no_such_condition"),
        ("BEGIN RAISE others; END", "others"),
        ("BEGIN RAISE \"Check_Violation\"; END", "Check_Violation"),
        (
            "BEGIN NULL; EXCEPTION WHEN no_such_condition THEN NULL; END",
            "no_such_condition",
        ),
        (
            "BEGIN NULL; EXCEPTION WHEN check_violation OR \"OTHERS\" THEN NULL; END",
            "OTHERS",
        ),
    ] {
        let error = embedded_sql(body).expect_err(body);
        assert_eq!(
            error.to_string(),
            format!("unrecognized exception condition \"{name}\""),
            "{body}"
        );
        assert!(matches!(error, Error::UnknownCondition(_)), "{body}");
    }
}

#[test]
fn a_routine_defined_inside_the_body_is_one_statement() {
    let body = "
        #variable_conflict use_column
        BEGIN
          CREATE OR REPLACE FUNCTION g() RETURNS INT LANGUAGE sql
            BEGIN ATOMIC SELECT 1; SELECT CASE WHEN true THEN 2 END; END;
          CREATE PROCEDURE p() LANGUAGE sql BEGIN ATOMIC SELECT 1; END;
          CREATE RULE r AS ON INSERT TO t DO ALSO (INSERT INTO a VALUES (1); INSERT INTO b VALUES (2));
          CALL p((SELECT 1 FROM docs));
          COMMIT;
        END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              CREATE OR REPLACE FUNCTION g() RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT 1; SELECT CASE WHEN true THEN 2 END; END
              CREATE PROCEDURE p () LANGUAGE sql BEGIN ATOMIC SELECT 1; END
              CREATE RULE r AS ON INSERT TO t DO ALSO (INSERT INTO a VALUES (1); INSERT INTO b VALUES (2))
              CALL p((SELECT 1 FROM docs))
              commit
            "
        )
    );
}

#[test]
fn every_raise_form_keeps_its_level_condition_format_and_named_options() {
    let body = "BEGIN
        RAISE 'plain %', (SELECT 1 FROM a);
        RAISE SQLSTATE '22012' USING MESSAGE = (SELECT 'm' FROM b);
        RAISE division_by_zero;
        RAISE EXCEPTION USING ERRCODE = 'P0001', DETAIL := (SELECT 'd' FROM c);
        RAISE USING MESSAGE = (SELECT 'u' FROM d);
        RAISE DEBUG 'debug';
        raise log 'log %', (SELECT 2 FROM e);
        RAISE INFO $$info$$;
        RAISE WARNING Unique_Violation;
        RAISE NOTICE USING MESSAGE = 'notice';
        RAISE \"raise_exception\" USING hint = 'h', column = 'c', constraint = 'k',
          datatype = 'd', table = 't', schema = 's';
        RAISE EXCEPTION 'denied' USING ERRCODE = 'insufficient_privilege';
        RAISE EXCEPTION 'denied' USING HINT = 'insufficient_privilege';
    END";
    assert_eq!(
        outline(body),
        expected(
            r#"
            block
              raise Exception format "plain %"
                argument SELECT (SELECT 1 FROM a)
              raise Exception sqlstate 22012
                option Message = SELECT (SELECT 'm' FROM b)
              raise Exception condition division_by_zero
              raise Exception
                option Errcode = SELECT 'P0001'
                option Detail = SELECT (SELECT 'd' FROM c)
              raise Exception
                option Message = SELECT (SELECT 'u' FROM d)
              raise Debug format "debug"
              raise Log format "log %"
                argument SELECT (SELECT 2 FROM e)
              raise Info format "info"
              raise Warning condition unique_violation
              raise Notice
                option Message = SELECT 'notice'
              raise Exception condition raise_exception
                option Hint = SELECT 'h'
                option Column = SELECT 'c'
                option Constraint = SELECT 'k'
                option Datatype = SELECT 'd'
                option Table = SELECT 't'
                option Schema = SELECT 's'
              raise Exception format "denied"
                option Errcode = SELECT 'insufficient_privilege'
              raise Exception format "denied"
                option Hint = SELECT 'insufficient_privilege'
            "#
        )
    );
}

#[test]
fn every_control_transfer_stays_where_it_is_written() {
    let body = r#"DECLARE s text; result text;
    BEGIN
      <<outer_loop>> LOOP
        LOOP
          s := 'before exit';
          EXIT Outer_Loop;
          s := 'after exit';
        END LOOP;
        s := 'outer tail';
      END LOOP;
      <<inner>> BEGIN
        EXIT inner WHEN s IS NULL;
      END;
      <<"Mixed">> FOR iteration IN 1..2 LOOP
        CONTINUE;
        CONTINUE "Mixed" WHEN iteration = 1;
        s := 'after continue';
      END LOOP;
      BEGIN
        IF level = 'exception' THEN RAISE EXCEPTION 'stop'; ELSE RAISE NOTICE 'go on'; END IF;
        ASSERT false;
        EXECUTE s INTO result;
        RETURN 'fell through: ' || result;
      EXCEPTION WHEN raise_exception THEN
        RETURN 'handler: ' || s;
      END;
    END"#;
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare s TEXT
              declare result TEXT
              <<outer_loop>> loop
                loop
                  s := SELECT 'before exit'
                  exit outer_loop
                  s := SELECT 'after exit'
                s := SELECT 'outer tail'
              <<inner>> block
                exit inner when SELECT s IS NULL
              <<Mixed>> range iteration in SELECT 1 .. SELECT 2
                continue
                continue Mixed when SELECT iteration = 1
                s := SELECT 'after continue'
              block
                if
                  when SELECT level = 'exception'
                    raise Exception format \"stop\"
                  else
                    raise Notice format \"go on\"
                assert SELECT false
                dynamic SELECT s
                  into result
                return SELECT 'fell through: ' || result
                handler condition raise_exception
                  return SELECT 'handler: ' || s
            "
        )
    );
}

#[test]
fn an_exit_or_continue_with_no_loop_to_reach_is_refused() {
    for (body, kind, label) in [
        ("BEGIN EXIT; END", ExitKind::Exit, None),
        ("BEGIN EXIT WHEN true; END", ExitKind::Exit, None),
        ("BEGIN LOOP NULL; END LOOP; EXIT; END", ExitKind::Exit, None),
        ("BEGIN CONTINUE; END", ExitKind::Continue, None),
        (
            "<<b>> BEGIN LOOP CONTINUE b; END LOOP; END",
            ExitKind::Continue,
            Some("b"),
        ),
        (
            "BEGIN LOOP CONTINUE nowhere; END LOOP; END",
            ExitKind::Continue,
            Some("nowhere"),
        ),
        (
            "BEGIN <<x>> LOOP <<x>> BEGIN CONTINUE x; END; END LOOP; END",
            ExitKind::Continue,
            Some("x"),
        ),
    ] {
        assert!(
            matches!(
                embedded_sql(body),
                Err(Error::NoEnclosingLoop { kind: found, label: ref found_label })
                    if found == kind && found_label.as_deref() == label
            ),
            "{body}: {:?}",
            embedded_sql(body)
        );
    }
    for (body, message) in [
        (
            "BEGIN EXIT; END",
            "EXIT without a label reaches no enclosing loop",
        ),
        (
            "<<b>> BEGIN LOOP CONTINUE b; END LOOP; END",
            "CONTINUE b reaches no enclosing loop",
        ),
    ] {
        let error = embedded_sql(body).expect_err("PostgreSQL refuses the body");
        assert_eq!(error.to_string(), message, "{body}");
    }
    for body in [
        "BEGIN LOOP BEGIN EXIT; END; END LOOP; END",
        "BEGIN LOOP BEGIN NULL; EXCEPTION WHEN others THEN CONTINUE; END; END LOOP; END",
        "<<b>> BEGIN NULL; EXCEPTION WHEN others THEN EXIT b; END",
        "BEGIN <<x>> BEGIN <<x>> LOOP CONTINUE x; END LOOP; END; END",
        "BEGIN EXIT f; END",
    ] {
        assert!(
            embedded_sql(body).is_ok(),
            "{body}: {:?}",
            embedded_sql(body)
        );
    }
}

#[test]
fn sibling_blocks_do_not_count_toward_the_nesting_limit() {
    let body = format!("BEGIN {}END", "BEGIN NULL; END; ".repeat(200));
    assert_eq!(
        embedded_sql(&body).map(|block| block.statements.len()).ok(),
        Some(200)
    );
}

#[test]
fn every_declaration_keeps_its_name_type_and_default() {
    let body = r#"
        DECLARE
          plain text;
          sized varchar(10)[];
          exact numeric(10, 2) = 1;
          zoned timestamp with time zone;
          qualified public.mood;
          column_value t.name%TYPE := p;
          row_value t%ROWTYPE;
          column_array public.t.name % TYPE[];
          row_array t%ROWTYPE[3][];
          constant_value CONSTANT text COLLATE "C" NOT NULL DEFAULT 'x';
          required int NOT NULL := 0;
          "Quoted" record;
          by_position ALIAS FOR $2;
          by_name ALIAS FOR f.p;
          free refcursor;
          bound NO SCROLL CURSOR (k t.name%TYPE, n INT) FOR SELECT * FROM t WHERE name = k;
        BEGIN
          EXECUTE 'SELECT ' || plain;
        END"#;
    assert_eq!(
        outline(body),
        expected(
            r#"
            block
              declare plain TEXT
              declare sized VARCHAR(10)[]
              declare exact NUMERIC(10,2) default SELECT 1
              declare zoned TIMESTAMP WITH TIME ZONE
              declare qualified public.mood
              declare column_value t.name%TYPE default SELECT p
              declare row_value t%ROWTYPE
              declare column_array public.t.name%TYPE[]
              declare row_array t%ROWTYPE[]
              declare constant_value constant TEXT collate "C" not null default SELECT 'x'
              declare required INT not null default SELECT 0
              declare Quoted record
              declare by_position alias for $2
              declare by_name alias for f.p
              declare free refcursor
              declare bound no scroll cursor (k t.name%TYPE, n INT)
              dynamic SELECT 'SELECT ' || plain
            "#
        )
    );
}

#[test]
fn a_type_the_parser_cannot_read_is_an_error_naming_it() {
    for (body, sql) in [
        ("DECLARE x %TYPE; BEGIN NULL; END", "% TYPE"),
        ("DECLARE x t.name%SIZE; BEGIN NULL; END", "t . name % SIZE"),
        (
            "DECLARE x integer integer; BEGIN NULL; END",
            "integer integer",
        ),
        ("DECLARE x text COLLATE 1; BEGIN NULL; END", "1"),
        ("DECLARE x ALIAS FOR 1; BEGIN NULL; END", "1"),
    ] {
        match embedded_sql(body) {
            Err(Error::EmbeddedSql { sql: found, .. }) => assert_eq!(found, sql, "{body}"),
            other => panic!("{body}: expected an embedded SQL error, got {other:?}"),
        }
    }
}

#[test]
fn an_unparsable_embedded_statement_is_an_error_naming_it() {
    let error = embedded_sql("BEGIN SELECT FROM WHERE; END").unwrap_err();
    match error {
        Error::EmbeddedSql { sql, .. } => assert_eq!(sql, "SELECT FROM WHERE"),
        other => panic!("expected an embedded SQL error, got {other}"),
    }
    for body in ["BEGIN x[1 := 2; END", "BEGIN CREATE FUNCTION f() END; END"] {
        assert!(
            matches!(embedded_sql(body), Err(Error::EmbeddedSql { .. })),
            "{body}: {:?}",
            embedded_sql(body)
        );
    }
}

#[test]
fn a_malformed_body_is_a_syntax_error() {
    for body in [
        "SELECT 1",
        "BEGIN IF x THEN NULL; END LOOP; END",
        "BEGIN NULL;",
        "BEGIN NULL; END; NULL",
        "BEGIN IF x; END IF; END",
        "DECLARE c CURSOR (k INT; BEGIN END",
        "BEGIN IF x) THEN NULL; END IF; END",
        "BEGIN IF x; NULL; END IF; IF y THEN NULL; END IF; END",
        "BEGIN NULL; END.",
        "BEGIN RAISE SQLSTATE 22012; END",
        "BEGIN IF x",
        "BEGIN SELECT 1",
        "<<top BEGIN NULL; END",
        "BEGIN <<inner>> NULL; END",
        "#option 1 BEGIN NULL; END",
        "BEGIN OPEN 1; END",
        "DECLARE c CURSOR SELECT 1; BEGIN NULL; END",
        "BEGIN FOR i IN REVERSE SELECT 1 LOOP NULL; END LOOP; END",
        "BEGIN EXECUTE 'SELECT 1' INTO; END",
        "BEGIN RAISE USING MESSAGE 'm'; END",
        "BEGIN RAISE USING ERRCOD = 'P0001'; END",
        "BEGIN RAISE USING \"MESSAGE\" = 'm'; END",
        "BEGIN RAISE SQLSTATE '2201'; END",
        "BEGIN RAISE SQLSTATE '22o12'; END",
        "BEGIN NULL; EXCEPTION WHEN THEN NULL; END",
        "BEGIN NULL; EXCEPTION WHEN others OR THEN NULL; END",
        "BEGIN NULL; EXCEPTION WHEN unique_violation b THEN NULL; END",
        "BEGIN NULL; EXCEPTION WHEN unique_violation.b THEN NULL; END",
        "BEGIN NULL; EXCEPTION WHEN SQLSTATE 22012 THEN NULL; END",
        "BEGIN NULL; EXCEPTION WHEN SQLSTATE '2201' THEN NULL; END",
        "BEGIN NULL; EXCEPTION WHEN 'P0001' THEN NULL; END",
        "BEGIN SELECT 1 INTO a INTO b; END",
        "BEGIN SELECT 1 INTO; END",
        "DECLARE x; BEGIN NULL; END",
        "DECLARE x CONSTANT := 1; BEGIN NULL; END",
        "DECLARE x text NOT NULL COLLATE \"C\" := 'a'; BEGIN NULL; END",
        "DECLARE x text COLLATE; BEGIN NULL; END",
        "DECLARE x ALIAS FOR; BEGIN NULL; END",
        "DECLARE x ALIAS $1; BEGIN NULL; END",
        "DECLARE c CURSOR (k) FOR SELECT 1; BEGIN NULL; END",
        "DECLARE c CURSOR (k INT,) FOR SELECT 1; BEGIN NULL; END",
        "BEGIN FOR a, b IN 1..2 LOOP NULL; END LOOP; END",
        "DECLARE c CURSOR FOR SELECT 1; BEGIN FOR a, b IN c LOOP NULL; END LOOP; END",
        "BEGIN FOR r IN EXECUTE 'SELECT 1' INTO n LOOP NULL; END LOOP; END",
        "BEGIN OPEN c FOR EXECUTE 'SELECT 1' INTO n; END",
        "DECLARE c CURSOR FOR SELECT 1; BEGIN OPEN c SCROLL; END",
        "DECLARE c CURSOR FOR SELECT 1; BEGIN OPEN c NO SCROLL; END",
        "DECLARE c refcursor; BEGIN OPEN c NO FOR SELECT 1; END",
        "BEGIN RETURN QUERY EXECUTE 'SELECT 1' INTO n; END",
        "BEGIN EXECUTE 'SELECT 1' INTO a INTO b; END",
        "BEGIN FETCH NEXT c INTO s; END",
        "BEGIN FETCH c; END",
        "BEGIN FETCH c INTO STRICT s; END",
        "BEGIN FETCH NEXT 2 FROM c INTO r; END",
        "BEGIN FETCH PRIOR 2 FROM c INTO r; END",
        "BEGIN FETCH FIRST 2 FROM c INTO r; END",
        "BEGIN FETCH LAST 2 FROM c INTO r; END",
        "BEGIN MOVE NEXT 2 FROM c; END",
        "BEGIN MOVE PRIOR 2 FROM c; END",
        "BEGIN MOVE FIRST 2 FROM c; END",
        "BEGIN MOVE LAST 2 FROM c; END",
        "BEGIN MOVE ALL 2 FROM c; END",
        "BEGIN MOVE FORWARD ALL 2 FROM c; END",
        "BEGIN MOVE BACKWARD ALL 2 FROM c; END",
        "BEGIN FETCH ABSOLUTE FROM c INTO r; END",
        "BEGIN FETCH RELATIVE IN c INTO r; END",
        "BEGIN MOVE ABSOLUTE IN c; END",
        "BEGIN MOVE RELATIVE FROM c; END",
        "BEGIN FETCH ALL FROM c INTO r; END",
        "BEGIN FETCH FORWARD ALL FROM c INTO r; END",
        "BEGIN FETCH BACKWARD ALL IN c INTO r; END",
        "BEGIN FETCH FORWARD 1 FROM c INTO r; END",
        "BEGIN FETCH BACKWARD 1 FROM c INTO r; END",
        "BEGIN FETCH 1 FROM c INTO r; END",
        "BEGIN GET CURRENT DIAGNOSTICS t = MESSAGE_TEXT; END",
        "BEGIN GET STACKED DIAGNOSTICS n = ROW_COUNT; END",
        "BEGIN GET DIAGNOSTICS n = ROW_COUNT n; END",
        "BEGIN GET DIAGNOSTICS n ROW_COUNT; END",
        "BEGIN GET DIAGNOSTICS n = \"ROW_COUNT\"; END",
        "BEGIN FOREACH x SLICE (1) IN ARRAY a LOOP NULL; END LOOP; END",
        "BEGIN x.* := 2; END",
        "BEGIN x[] := 2; END",
    ] {
        assert!(
            matches!(embedded_sql(body), Err(Error::Syntax { .. })),
            "{body}: {:?}",
            embedded_sql(body)
        );
    }
}

#[test]
fn a_malformed_close_commit_or_rollback_is_a_syntax_error() {
    for body in [
        "BEGIN CLOSE; END",
        "BEGIN CLOSE 1; END",
        "BEGIN CLOSE c extra; END",
        "BEGIN CLOSE c[1]; END",
        "BEGIN COMMIT AND; END",
        "BEGIN COMMIT CHAIN; END",
        "BEGIN COMMIT AND NO; END",
        "BEGIN COMMIT AND CHAIN extra; END",
        "BEGIN COMMIT AND NO CHAIN extra; END",
        "BEGIN ROLLBACK AND; END",
        "BEGIN ROLLBACK CHAIN; END",
        "BEGIN ROLLBACK AND NO; END",
        "BEGIN ROLLBACK AND CHAIN extra; END",
        "BEGIN ROLLBACK AND NO CHAIN extra; END",
    ] {
        assert!(
            matches!(embedded_sql(body), Err(Error::Syntax { .. })),
            "{body}: {:?}",
            embedded_sql(body)
        );
    }
}

#[test]
fn an_unterminated_string_is_an_untokenizable_body() {
    assert!(matches!(
        embedded_sql("BEGIN x := 'open; END"),
        Err(Error::UntokenizableBody { .. })
    ));
}

#[test]
fn opening_a_cursor_no_block_binds_is_an_error() {
    for body in [
        "BEGIN OPEN c; END",
        "DECLARE c refcursor; BEGIN OPEN c; END",
        "DECLARE c CURSOR FOR SELECT 1; BEGIN DECLARE c INT; BEGIN OPEN c; END; END",
        "DECLARE c CURSOR FOR SELECT 1; BEGIN FOR c IN 1..2 LOOP OPEN c; END LOOP; END",
        "DECLARE sqlstate CURSOR FOR SELECT 1;
        BEGIN NULL; EXCEPTION WHEN others THEN OPEN sqlstate; END",
    ] {
        assert!(
            matches!(embedded_sql(body), Err(Error::UnboundCursor { ref name }) if name == "c" || name == "sqlstate"),
            "{body}: {:?}",
            embedded_sql(body)
        );
    }
}

#[test]
fn a_cursor_is_reached_through_an_alias_and_through_its_block_label() {
    let body = "
        <<outer>> DECLARE c CURSOR FOR SELECT 1 FROM outer_docs; d ALIAS FOR c;
        BEGIN
          DECLARE c CURSOR FOR SELECT 2 FROM inner_docs;
          BEGIN
            OPEN outer.c;
            OPEN d;
            FOR r IN outer.c LOOP NULL; END LOOP;
            FOR r IN d LOOP NULL; END LOOP;
          END;
        END";
    assert_eq!(
        outline(body),
        expected(
            "
            <<outer>> block
              declare c cursor
              declare d alias for c
              block
                declare c cursor
                open outer.c: SELECT 1 FROM outer_docs
                open d: SELECT 1 FROM outer_docs
                cursor r in outer.c: SELECT 1 FROM outer_docs
                cursor r in d: SELECT 1 FROM outer_docs
            "
        )
    );
    assert!(matches!(
        embedded_sql("<<b>> DECLARE c CURSOR FOR SELECT 1; BEGIN OPEN a.c; END"),
        Err(Error::UnboundCursor { ref name }) if name == "a.c"
    ));
}

#[test]
fn a_cursor_name_matches_by_postgresql_identifier_rules() {
    let body = r#"DECLARE "Mixed" CURSOR FOR SELECT 1 FROM quoted; plain CURSOR FOR SELECT 2 FROM folded;
        BEGIN OPEN "Mixed"; OPEN PLAIN; END"#;
    assert_eq!(
        outline(body),
        expected(
            r#"
            block
              declare Mixed cursor
              declare plain cursor
              open "Mixed": SELECT 1 FROM quoted
              open PLAIN: SELECT 2 FROM folded
            "#
        )
    );
    assert!(matches!(
        embedded_sql(r#"DECLARE "Mixed" CURSOR FOR SELECT 1; BEGIN OPEN mixed; END"#),
        Err(Error::UnboundCursor { .. })
    ));
}

#[test]
fn every_diagnostics_item_is_kept_with_its_target() {
    let body = "BEGIN
        GET DIAGNOSTICS a = ROW_COUNT, b := pg_routine_oid, outer_block.c = PG_CONTEXT;
        GET CURRENT DIAGNOSTICS d = ROW_COUNT;
    EXCEPTION WHEN others THEN
        GET STACKED DIAGNOSTICS
          e = RETURNED_SQLSTATE, f = COLUMN_NAME, g = CONSTRAINT_NAME, h = PG_DATATYPE_NAME,
          i = MESSAGE_TEXT, j = TABLE_NAME, k = SCHEMA_NAME, l = PG_EXCEPTION_DETAIL,
          m = PG_EXCEPTION_HINT, n = PG_EXCEPTION_CONTEXT, r.o = PG_CONTEXT;
    END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              diagnostics
                a = RowCount
                b = RoutineOid
                outer_block.c = Context
              diagnostics
                d = RowCount
              handler others
                diagnostics stacked
                  e = ReturnedSqlstate
                  f = ColumnName
                  g = ConstraintName
                  h = DatatypeName
                  i = MessageText
                  j = TableName
                  k = SchemaName
                  l = ExceptionDetail
                  m = ExceptionHint
                  n = ExceptionContext
                  r.o = Context
            "
        )
    );
}

#[test]
fn a_call_keeps_its_arguments_in_its_sql() {
    let block = embedded_sql("DECLARE s text; BEGIN CALL p(s); END").expect("a CALL reads");
    let [
        Node {
            kind: NodeKind::Statement { statement, into },
            ..
        },
    ] = block.statements.as_slice()
    else {
        panic!("expected one statement, got {:?}", block.statements);
    };
    assert!(into.is_none());
    let Statement::Call(call) = statement.as_ref() else {
        panic!("expected a CALL, got {statement}");
    };
    let FunctionArguments::List(list) = &call.args else {
        panic!("expected an argument list, got {}", call.args);
    };
    assert_eq!(list.args.len(), 1);
    assert_eq!(list.args[0].to_string(), "s");
}

#[test]
fn a_parameter_written_as_its_number_is_a_target() {
    let body = "DECLARE c CURSOR FOR SELECT 1;
    BEGIN
        $1 := 3;
        $1.f := 4;
        SELECT 1, 2 INTO a, $2;
        OPEN c;
        FETCH c INTO $2;
        GET DIAGNOSTICS $1 = ROW_COUNT;
        FOR $1 IN SELECT 3 LOOP NULL; END LOOP;
        FOREACH $1 IN ARRAY ARRAY[4] LOOP NULL; END LOOP;
    END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare c cursor
              $1 := SELECT 3
              $1.f := SELECT 4
              SELECT 1, 2
                into a, $2
              open c: SELECT 1
              fetch forward one c into $2
              diagnostics
                $1 = RowCount
              query $1 in SELECT 3
              array $1 slice 0 in SELECT ARRAY[4]
            "
        )
    );
}

#[test]
fn a_statement_keyword_followed_by_an_assignment_is_a_variable() {
    let body = "BEGIN get := 4; open.f = 5; fetch[1] := 6; GET DIAGNOSTICS get = ROW_COUNT; END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              get := SELECT 4
              open.f := SELECT 5
              fetch[SELECT 1] := SELECT 6
              diagnostics
                get = RowCount
            "
        )
    );
}

#[test]
fn nesting_past_the_limit_is_an_error_instead_of_a_stack_overflow() {
    let depth = 10_000;
    let body = format!("{}NULL;{}", "BEGIN ".repeat(depth), " END;".repeat(depth));
    assert!(matches!(
        embedded_sql(&body),
        Err(Error::NestingTooDeep { .. })
    ));
}

#[test]
fn nodes_and_the_sql_they_embed_carry_where_the_body_writes_them() {
    use sqlparser_plpgsql::embedded::expression;

    let body = "<<outer>> BEGIN
  FOR i IN 1..10 LOOP
    x.f := i + 1;  -- a..b stays as written
  END LOOP;
END";
    let block = embedded_sql(body).expect("the body reads");
    let [looped] = block.statements.as_slice() else {
        panic!("expected one loop, got {:?}", block.statements);
    };
    assert_eq!(
        quoted(body, looped.span),
        "FOR i IN 1..10 LOOP\n    x.f := i + 1;  -- a..b stays as written\n  END LOOP;"
    );
    let NodeKind::Loop(Loop {
        control: LoopControl::Range { lower, upper, .. },
        statements,
        ..
    }) = &looped.kind
    else {
        panic!("expected a range loop, got {looped:?}");
    };
    assert_eq!(quoted(body, expression(lower).span()), "1");
    assert_eq!(quoted(body, expression(upper).span()), "10");
    let [assigned] = statements.as_slice() else {
        panic!("expected one assignment, got {statements:?}");
    };
    assert_eq!(quoted(body, assigned.span), "x.f := i + 1;");
    let NodeKind::Assignment(assignment) = &assigned.kind else {
        panic!("expected an assignment, got {assigned:?}");
    };
    assert_eq!(quoted(body, assignment.target.name.span()), "x.f");
    assert_eq!(quoted(body, expression(&assignment.value).span()), "i + 1");
}

#[test]
fn the_same_steps_written_elsewhere_compare_equal() {
    let read = |body: &str| embedded_sql(body).unwrap_or_else(|error| panic!("{body}: {error}"));
    let compact = read("BEGIN x := 1; END");
    let spread = read("BEGIN\n  x   :=   1;\nEND");
    assert_ne!(compact.statements[0].span, spread.statements[0].span);
    assert_eq!(compact, spread);
    assert_ne!(compact, read("BEGIN x := 2; END"));
}
