//! The SQL a PL/pgSQL body embeds, read through the public API.
#![cfg(feature = "body-parse")]

use core::fmt::Write as _;

use sqlparser_plpgsql::{
    embedded::{
        AliasTarget, Block, BoundCursor, Branch, DeclarationKind, DeclaredType, Dynamic, Error,
        LoopControl, Node,
    },
    embedded_sql,
};

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
        line(out, depth + 1, "handler");
        write_nodes(out, handler, depth + 2);
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
        DeclarationKind::Cursor(cursor) if cursor.parameters.is_empty() => "cursor".into(),
        DeclarationKind::Cursor(cursor) => {
            let parameters: Vec<String> = cursor
                .parameters
                .iter()
                .map(|parameter| format!("{} {}", parameter.name, type_text(&parameter.data_type)))
                .collect();
            format!("cursor ({})", parameters.join(", "))
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
        &format!("{opened} {}: {}", cursor.name, cursor.query),
    );
    for argument in &cursor.arguments {
        line(out, depth + 1, &format!("argument {argument}"));
    }
}

fn write_branches(out: &mut String, branches: &[Branch], otherwise: &[Node], depth: usize) {
    for branch in branches {
        line(out, depth, &format!("when {}", branch.condition));
        write_nodes(out, &branch.statements, depth + 1);
    }
    if !otherwise.is_empty() {
        line(out, depth, "else");
        write_nodes(out, otherwise, depth + 1);
    }
}

fn write_nodes(out: &mut String, nodes: &[Node], depth: usize) {
    for node in nodes {
        match node {
            Node::Statement(statement) => line(out, depth, &statement.to_string()),
            Node::Dynamic(dynamic) => write_dynamic(out, dynamic, depth),
            Node::Block(block) => write_block(out, block, depth),
            Node::Open(cursor) => write_bound_cursor(out, "open", cursor, depth),
            Node::If(conditional) => {
                line(out, depth, "if");
                write_branches(
                    out,
                    &conditional.branches,
                    &conditional.otherwise,
                    depth + 1,
                );
            }
            Node::Case(case) => {
                match &case.subject {
                    Some(subject) => line(out, depth, &format!("case {subject}")),
                    None => line(out, depth, "case"),
                }
                write_branches(out, &case.branches, &case.otherwise, depth + 1);
            }
            Node::Loop(looped) => {
                let label = looped.label.as_deref();
                match &looped.control {
                    LoopControl::Unconditional => line(out, depth, &labelled(label, "loop")),
                    LoopControl::While(condition) => {
                        line(out, depth, &labelled(label, &format!("while {condition}")));
                    }
                    LoopControl::Range {
                        variable,
                        lower,
                        upper,
                        step,
                    } => {
                        let text = format!("range {variable} in {lower} .. {upper}");
                        line(out, depth, &labelled(label, &text));
                        if let Some(step) = step {
                            line(out, depth + 1, &format!("by {step}"));
                        }
                    }
                    LoopControl::Query(query) => {
                        line(out, depth, &labelled(label, &format!("query {query}")));
                    }
                    LoopControl::Cursor { variable, cursor } => {
                        let opened = labelled(label, &format!("cursor {variable} in"));
                        write_bound_cursor(out, &opened, cursor, depth);
                    }
                    LoopControl::Dynamic(dynamic) => {
                        line(out, depth, &labelled(label, "loop over"));
                        write_dynamic(out, dynamic, depth + 1);
                    }
                    LoopControl::Array(array) => {
                        line(out, depth, &labelled(label, &format!("array {array}")));
                    }
                }
                write_nodes(out, &looped.statements, depth + 1);
            }
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
              SELECT 1 FROM pp_docs
              query SELECT id FROM pp_docs
                INSERT INTO pp_log VALUES (r.id)
              open c: SELECT id FROM pp_docs
              block
                UPDATE pp_docs SET id = id
                handler
                  DELETE FROM pp_log
              SELECT id FROM pp_docs
            "
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
              SELECT p_file_id
            "
        )
    );
}

#[test]
fn into_targets_are_removed_wherever_postgresql_reads_them() {
    let body = "DECLARE n INT; x INT; y INT; r RECORD;
    BEGIN
        SELECT count(*) INTO n FROM docs;
        SELECT INTO x, y a, b FROM docs;
        SELECT * INTO STRICT r FROM docs WHERE id = 1;
        INSERT INTO docs VALUES (1) RETURNING id INTO n;
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
              SELECT a, b FROM docs
              SELECT * FROM docs WHERE id = 1
              INSERT INTO docs VALUES (1) RETURNING id
              MERGE INTO docs USING src ON docs.id = src.id WHEN MATCHED THEN DELETE
              SELECT 1 FROM docs
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
              SELECT a FROM docs WHERE id = 1
              SELECT (SELECT max(id) FROM docs)
              SELECT (SELECT 1 FROM docs)
              SELECT 2
              SELECT 5
              SELECT 1
              SELECT 2
              SELECT ARRAY[1, 2]
              SELECT arr[1]
              SELECT 3
              SELECT total
              SELECT (SELECT 1 FROM logs)
              SELECT (SELECT 'h' FROM hints)
              SELECT total > 0
              SELECT (SELECT 'bad' FROM messages)
              SELECT (SELECT true FROM checks)
              SELECT (SELECT 2 FROM docs)
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
          FOR a, b IN WITH w AS (SELECT 1, 2) SELECT * FROM w LOOP NULL; END LOOP;
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
                SELECT (SELECT true FROM stop)
              while SELECT (SELECT count(*) FROM queue) > 0
                DELETE FROM queue
              range i in SELECT 1 .. SELECT 10
                SELECT i = 2
              range i in SELECT (SELECT max(id) FROM docs) .. SELECT lo
                by SELECT 2
              cursor r in c: SELECT id FROM docs WHERE id = key
                argument SELECT 7
              cursor r in c: SELECT id FROM docs WHERE id = key
                argument SELECT v
              cursor r in pair: SELECT id FROM docs WHERE id BETWEEN first_id AND last_id
                argument SELECT 1
                argument SELECT (SELECT 2 FROM bounds)
              cursor r in plain: SELECT id FROM plain_docs
              array SELECT x
              array SELECT (SELECT array_agg(id) FROM docs)
              query WITH w AS (SELECT 1, 2) SELECT * FROM w
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
          OPEN c;
        END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              declare c cursor
              declare never cursor
              declare d cursor (k INT)
              declare free refcursor
              declare n INT
              if
                when SELECT flag
                  open c: SELECT id FROM outer_docs
              block
                declare c cursor
                open c: SELECT id FROM inner_docs
              open d: SELECT id FROM keyed WHERE id = k
                argument SELECT (SELECT 1 FROM keys)
              SELECT id FROM loose
              SELECT (SELECT 2 FROM steps)
              open c: SELECT id FROM outer_docs
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
              dynamic SELECT format('SELECT %I', c)
                using SELECT d
              dynamic SELECT 'SELECT 1'
              dynamic SELECT 'SELECT 2'
                using SELECT e
              loop over
                dynamic SELECT 'SELECT 3'
                  using SELECT f
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
                handler
                  INSERT INTO errors VALUES (1)
                handler
                  block
                    handler
              UPDATE docs SET v = 1
              handler
                DELETE FROM docs
            "
        )
    );
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
            "
        )
    );
}

#[test]
fn every_raise_form_yields_the_expressions_it_evaluates() {
    let body = "BEGIN
        RAISE 'plain %', (SELECT 1 FROM a);
        RAISE SQLSTATE '22012' USING MESSAGE = (SELECT 'm' FROM b);
        RAISE division_by_zero;
        RAISE EXCEPTION USING ERRCODE = 'P0001', DETAIL := (SELECT 'd' FROM c);
        RAISE USING MESSAGE = (SELECT 'u' FROM d);
    END";
    assert_eq!(
        outline(body),
        expected(
            "
            block
              SELECT (SELECT 1 FROM a)
              SELECT (SELECT 'm' FROM b)
              SELECT 'P0001'
              SELECT (SELECT 'd' FROM c)
              SELECT (SELECT 'u' FROM d)
            "
        )
    );
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
              declare bound cursor (k t.name%TYPE, n INT)
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
fn nesting_past_the_limit_is_an_error_instead_of_a_stack_overflow() {
    let depth = 10_000;
    let body = format!("{}NULL;{}", "BEGIN ".repeat(depth), " END;".repeat(depth));
    assert!(matches!(
        embedded_sql(&body),
        Err(Error::NestingTooDeep { .. })
    ));
}
