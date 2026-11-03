//! Rewrites PL/pgSQL function bodies into shapes sqlparser accepts: `:=`
//! assignments, DECLARE blocks, and SELECT INTO.

use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::fmt::Write;

use super::{
    context::{PlPgSqlContext, VariableBinding, VariableDeclaration},
    scanner::Scanner,
};
use crate::Error;

/// Preprocessor for PL/pgSQL function bodies.
pub struct PlPgSqlPreprocessor;

/// Rewrite a dollar-quoted default as a single-quoted one, the only string
/// literal SQLite has. Anything else passes through.
fn single_quoted_default(default: &str) -> String {
    let Some(body) = dollar_quoted_body(default) else {
        return default.to_string();
    };
    format!("'{}'", body.replace('\'', "''"))
}

/// The text between a dollar-quoted string's delimiters, when `text` is exactly
/// one such string.
fn dollar_quoted_body(text: &str) -> Option<&str> {
    let tag_end = text.strip_prefix('$')?.find('$')? + 1;
    let delimiter = &text[..=tag_end];
    if !delimiter[1..tag_end]
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let body = text.get(delimiter.len()..)?.strip_suffix(delimiter)?;
    Some(body)
}

/// Refuses a statement that assigns to a qualified target.
fn reject_qualified_assignment(statement: &str) -> Result<(), Error> {
    let Some((qualifier, name)) = qualified_assignment_target(statement) else {
        return Ok(());
    };
    Err(Error::QualifiedAssignment { qualifier, name })
}

/// The `(qualifier, name)` a statement assigns to, when its target is
/// qualified.
///
/// `:=` only ever assigns, so a qualified name before one is conclusive
/// anywhere. A lone `=` also compares, so it counts only where a statement can
/// start.
fn qualified_assignment_target(statement: &str) -> Option<(String, String)> {
    qualified_walrus_target(statement).or_else(|| qualified_equals_target(statement))
}

/// The qualified target of a `:=` assignment anywhere in `statement`.
///
/// Read back from the operator exactly as the rewrite reads it, so the two
/// agree on the target. A named function argument also spells `:=`, but its
/// name is never qualified.
fn qualified_walrus_target(statement: &str) -> Option<(String, String)> {
    let mut from = 0;
    while let Some(offset) = Scanner::new(&statement[from..]).find_str_in_code(":=") {
        let operator = from + offset;
        if let Some(target) = qualified_name_before(statement, operator) {
            return Some(target);
        }
        from = operator + 2;
    }
    None
}

/// The qualified target of a bare `=` assignment at a statement start.
///
/// A statement starts at the front of the chunk or just past `BEGIN`, `THEN`,
/// `ELSE`, or `LOOP`. A `THEN` or `ELSE` inside an open `CASE` belongs to the
/// expression, which keeps `CASE WHEN c THEN t.n = 1 ELSE FALSE END` a
/// comparison.
fn qualified_equals_target(statement: &str) -> Option<(String, String)> {
    statement_starts(statement)
        .into_iter()
        .find_map(|start| qualified_name_at_with_equals(statement, start))
}

/// What a keyword occurrence does to the scan for statement starts.
#[derive(Clone, Copy)]
enum Mark {
    /// `CASE`, whose arms are expression parts rather than statements.
    OpensCase,
    /// `END`, which closes the innermost `CASE`.
    ClosesCase,
    /// A keyword a statement can follow.
    OpensStatement,
}

/// Offsets in `statement` where a plpgsql statement can begin.
fn statement_starts(statement: &str) -> Vec<usize> {
    const KEYWORDS: [(&str, Mark); 6] = [
        ("CASE", Mark::OpensCase),
        ("END", Mark::ClosesCase),
        ("BEGIN", Mark::OpensStatement),
        ("THEN", Mark::OpensStatement),
        ("ELSE", Mark::OpensStatement),
        ("LOOP", Mark::OpensStatement),
    ];

    let scanner = Scanner::new(statement);
    let mut marks = Vec::new();
    for (keyword, mark) in KEYWORDS {
        let mut from = 0;
        while let Some(position) = scanner.find_keyword(keyword, from) {
            from = position + keyword.len();
            marks.push((position, mark, from));
        }
    }
    marks.sort_unstable_by_key(|&(position, ..)| position);

    let mut starts = vec![0];
    let mut case_depth = 0_usize;
    for (_, mark, after) in marks {
        match mark {
            Mark::OpensCase => case_depth += 1,
            // Saturating because a chunk can carry a closing `END IF` with no
            // `CASE` of its own, the caller having split at the semicolon
            // before it.
            Mark::ClosesCase => case_depth = case_depth.saturating_sub(1),
            Mark::OpensStatement if case_depth == 0 => starts.push(after),
            Mark::OpensStatement => {}
        }
    }
    starts
}

/// True for a character an unquoted identifier can carry.
///
/// One definition rather than one per reader, since a byte test and a
/// character test would disagree on a non-ASCII name.
fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Where the identifier that ends `text` begins, or `text.len()` when `text`
/// does not end in one.
///
/// Walks back over characters rather than searching for the last non-identifier
/// one, whose offset plus one can land inside a multi-byte character.
fn trailing_identifier_start(text: &str) -> usize {
    text.char_indices()
        .rev()
        .take_while(|&(_, character)| is_identifier_char(character))
        .last()
        .map_or(text.len(), |(index, _)| index)
}

/// Where the identifier starting at `from` ends, which is `from` when none
/// starts there.
fn identifier_end(text: &str, from: usize) -> usize {
    text[from..]
        .find(|character: char| !is_identifier_char(character))
        .map_or(text.len(), |offset| from + offset)
}

/// The first offset at or after `from` that is not ASCII whitespace.
///
/// ASCII rather than Unicode, since SQL does not treat a non-breaking space as
/// a separator.
fn skip_ascii_whitespace(text: &str, from: usize) -> usize {
    text[from..]
        .find(|character: char| !character.is_ascii_whitespace())
        .map_or(text.len(), |offset| from + offset)
}

/// Reads `<qualifier>.<name>` followed by a lone `=` at `from`.
///
/// The `=` must stand alone, so `==` and the `=>` of a named argument are both
/// rejected.
fn qualified_name_at_with_equals(statement: &str, from: usize) -> Option<(String, String)> {
    let qualifier_start = skip_ascii_whitespace(statement, from);
    let qualifier_end = identifier_end(statement, qualifier_start);
    if qualifier_end == qualifier_start || !statement[qualifier_end..].starts_with('.') {
        return None;
    }

    let name_start = qualifier_end + '.'.len_utf8();
    let name_end = identifier_end(statement, name_start);
    if name_end == name_start {
        return None;
    }

    let operator = &statement[skip_ascii_whitespace(statement, name_end)..];
    if !operator.starts_with('=') || operator.starts_with("==") || operator.starts_with("=>") {
        return None;
    }

    Some((
        statement[qualifier_start..qualifier_end].to_string(),
        statement[name_start..name_end].to_string(),
    ))
}

/// Reads `<qualifier>.<name>` backwards from `operator`.
fn qualified_name_before(statement: &str, operator: usize) -> Option<(String, String)> {
    let before = statement[..operator].trim_end();
    let (prefix, name) = before.split_at(trailing_identifier_start(before));
    if name.is_empty() {
        return None;
    }

    let qualifier_text = prefix.strip_suffix('.')?;
    let qualifier = &qualifier_text[trailing_identifier_start(qualifier_text)..];

    (!qualifier.is_empty()).then(|| (qualifier.to_string(), name.to_string()))
}

impl PlPgSqlPreprocessor {
    /// True when `body` opens an exception handler.
    ///
    /// A handler is `EXCEPTION WHEN`, which is what separates it from the
    /// `RAISE EXCEPTION` statement that shares the keyword. Read through the
    /// scanner, so neither one inside a literal or a comment counts.
    #[must_use]
    pub fn has_exception_handler(body: &str) -> bool {
        let scanner = Scanner::new(body);
        let mut from = 0;
        while let Some(position) = scanner.find_keyword("EXCEPTION", from) {
            let after = position + "EXCEPTION".len();
            if scanner
                .next_word_in_code(after)
                .is_some_and(|word| word.eq_ignore_ascii_case("when"))
            {
                return true;
            }
            from = after;
        }
        false
    }

    /// Preprocesses a PL/pgSQL function body string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::QualifiedAssignment`] when a statement assigns to
    /// a qualified target, such as `NEW.col`, outside the one shape that is
    /// translatable.
    pub fn preprocess(body: &str) -> Result<(String, PlPgSqlContext), Error> {
        let mut context = PlPgSqlContext::new();

        let (declare_section, body_section) = Self::split_declare_and_body(body);

        if let Some(declare) = declare_section {
            Self::parse_declarations(&declare, &mut context);
        }

        let transformed = Self::transform_body(&body_section, &context)?;

        Ok((transformed, context))
    }

    fn split_declare_and_body(body: &str) -> (Option<String>, String) {
        let scanner = Scanner::new(body);
        let declare_pos = scanner.find_keyword("DECLARE", 0);
        let begin_pos = scanner.find_keyword("BEGIN", 0);

        match (declare_pos, begin_pos) {
            // MUTANT: equivalent: `<=` cannot differ, since DECLARE and BEGIN
            // are different keywords and never share an offset.
            (Some(d), Some(b)) if d < b => (
                Some(body[d + "DECLARE".len()..b].trim().to_string()),
                body[b..].to_string(),
            ),
            (_, Some(b)) => (None, body[b..].to_string()),
            _ => (None, body.to_string()),
        }
    }

    fn parse_declarations(declare_section: &str, context: &mut PlPgSqlContext) {
        for decl in Scanner::new(declare_section).split_in_code(b';') {
            let decl = decl.trim();
            if decl.is_empty() {
                continue;
            }

            if let Some(var_decl) = Self::parse_single_declaration(decl) {
                context.add_declaration(var_decl);
            }
        }
    }

    fn parse_single_declaration(decl: &str) -> Option<VariableDeclaration> {
        let decl = decl.trim();
        if decl.is_empty() {
            return None;
        }

        let (name_type, default_value) = if let Some(pos) = decl.find(":=") {
            (decl[..pos].trim(), Some(decl[pos + 2..].trim()))
        } else if let Some(pos) = Scanner::new(decl).find_keyword("DEFAULT", 0) {
            (
                decl[..pos].trim(),
                Some(decl[pos + "DEFAULT".len()..].trim()),
            )
        } else {
            (decl, None)
        };
        let default_value = default_value.map(single_quoted_default);

        let parts: Vec<&str> = name_type.split_whitespace().collect();
        if parts.len() >= 2 {
            Some(VariableDeclaration {
                name: parts[0].to_string(),
                data_type: parts[1..].join(" "),
                default_value,
            })
        } else {
            None
        }
    }

    fn transform_body(body: &str, context: &PlPgSqlContext) -> Result<String, Error> {
        // Requoting first means every later transform sees an ordinary string
        // rather than a span each would have to learn about.
        let mut result = Scanner::new(body).requote_dollar_literals();

        // sqlparser spells it ELSEIF.
        result = Self::transform_elsif(&result);

        result = Self::transform_assignments(&result, context)?;

        result = Self::transform_select_into(&result, context);

        Self::transform_raise_statements(&result)
    }

    /// Rewrites RAISE statements, scanning body text directly so one inside an
    /// IF or THEN block is transformed too.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedRaiseUsing`] when a RAISE EXCEPTION
    /// USING clause is present but not in the single-item `MESSAGE =
    /// '<literal>'` form that translates cleanly, and
    /// [`Error::RaiseParameterCount`] when a format's placeholders and its
    /// arguments differ in number.
    fn transform_raise_statements(body: &str) -> Result<String, Error> {
        let mut result = String::new();
        let mut search_from = 0;
        let scanner = Scanner::new(body);

        while let Some(raise_pos) = scanner.find_keyword("RAISE", search_from) {
            let after_keyword = raise_pos + "RAISE".len();
            // One separator character follows the keyword. Stepping by
            // character keeps a multi-byte one from splitting a slice.
            let Some(separator) = body[after_keyword..].chars().next() else {
                break;
            };
            let after_raise_start = after_keyword + separator.len_utf8();
            let rest = &body[after_raise_start..];

            let (level_bytes, is_exception) = if Self::starts_with_ignoring_case(rest, "EXCEPTION")
            {
                ("EXCEPTION".len(), true)
            } else if Self::starts_with_ignoring_case(rest, "NOTICE")
                || Self::starts_with_ignoring_case(rest, "WARNING")
                || Self::starts_with_ignoring_case(rest, "INFO ")
                || Self::starts_with_ignoring_case(rest, "INFO\n")
                || Self::starts_with_ignoring_case(rest, "INFO;")
                || Self::starts_with_ignoring_case(rest, "DEBUG")
                || Self::starts_with_ignoring_case(rest, "LOG")
            {
                // MUTANT: equivalent: `!=` only shifts where an informational
                // level ends, and the whole statement is dropped either way.
                let len = rest
                    .find(|c: char| c.is_whitespace() || c == ';')
                    .unwrap_or(rest.len());
                (len, false)
            } else {
                result.push_str(&body[search_from..after_raise_start]);
                search_from = after_raise_start;
                continue;
            };

            result.push_str(&body[search_from..raise_pos]);

            let after_level = after_raise_start + level_bytes;
            let stmt_end =
                Self::find_unquoted_semicolon(&body[after_level..]).map(|p| after_level + p);
            let content_end = stmt_end.unwrap_or(body.len());

            if is_exception {
                let message_content = body[after_level..content_end].trim();
                let msg = if let Some(lit) = Self::extract_using_message_literal(message_content)? {
                    lit.to_string()
                } else {
                    Self::raise_message(message_content)?
                };
                result.push_str("SELECT RAISE(ABORT, ");
                result.push_str(&msg);
                result.push(')');
                result.push(';');
            }
            // Informational levels are dropped.

            search_from = stmt_end.map_or(body.len(), |end| end + 1);
        }

        result.push_str(&body[search_from..]);
        Ok(result)
    }

    /// The SQLite message expression for the RAISE arguments `'<format>'[,
    /// <argument> ...]`, each `%` of the format replaced by the next argument
    /// and `%%` by one `%`, as PostgreSQL formats the message.
    ///
    /// A NULL argument prints `<NULL>`, PostgreSQL's spelling. Options after a
    /// top-level `USING` are not arguments. A message that does not open with a
    /// string literal is returned as found.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RaiseParameterCount`] when the format's placeholders
    /// and the arguments differ in number.
    fn raise_message(content: &str) -> Result<String, Error> {
        let format = Self::extract_first_string_literal(content);
        if format.len() < 2 || !format.starts_with('\'') || !format.ends_with('\'') {
            return Ok(format.to_string());
        }
        let rest = &content[format.len()..];
        let rest =
            Self::find_top_level_keyword(rest, "USING", 0).map_or(rest, |using| &rest[..using]);
        let arguments = rest
            .trim_start()
            .strip_prefix(',')
            .map(Self::split_top_level_csv)
            .unwrap_or_default();

        // The text before the first placeholder, then the text after each.
        let mut leading = String::new();
        let mut trailing: Vec<String> = Vec::new();
        let mut chars = format[1..format.len() - 1].chars().peekable();
        while let Some(character) = chars.next() {
            if character == '%' && chars.next_if_eq(&'%').is_none() {
                trailing.push(String::new());
            } else {
                trailing.last_mut().unwrap_or(&mut leading).push(character);
            }
        }
        if trailing.len() != arguments.len() {
            return Err(Error::RaiseParameterCount {
                format: format.to_string(),
                placeholders: trailing.len(),
                arguments: arguments.len(),
            });
        }

        let mut message = String::with_capacity(
            content.len() + arguments.len() * " || coalesce(, '<NULL>') || ".len(),
        );
        if !leading.is_empty() || trailing.is_empty() {
            let _ = write!(message, "'{leading}'");
        }
        for (argument, piece) in arguments.iter().zip(&trailing) {
            if !message.is_empty() {
                message.push_str(" || ");
            }
            let _ = write!(message, "coalesce({argument}, '<NULL>')");
            if !piece.is_empty() {
                let _ = write!(message, " || '{piece}'");
            }
        }
        Ok(message)
    }

    /// True when `text` starts with the ASCII `prefix`, ignoring case.
    ///
    /// Returns false rather than panicking when `prefix.len()` lands inside a
    /// multi-byte character.
    fn starts_with_ignoring_case(text: &str, prefix: &str) -> bool {
        text.get(..prefix.len())
            .is_some_and(|found| found.eq_ignore_ascii_case(prefix))
    }

    /// Returns the byte offset of the first unquoted semicolon in `s`.
    fn find_unquoted_semicolon(s: &str) -> Option<usize> {
        Scanner::new(s).find_in_code(b';')
    }

    /// Extracts the first string literal from a comma-separated argument list.
    ///
    /// For `'hello', arg1, arg2` returns `'hello'`.
    /// If the first argument is not a string literal, returns `'error'` as a
    /// safe fallback.
    fn extract_first_string_literal(args: &str) -> &str {
        let trimmed = args.trim();
        if let Some(stripped) = trimmed.strip_prefix('\'') {
            // A doubled quote is an escape, not the close.
            let mut chars = stripped.char_indices();
            while let Some((i, c)) = chars.next() {
                if c == '\'' {
                    match chars.next() {
                        Some((_, '\'')) => {}          // escaped quote, keep going
                        _ => return &trimmed[..i + 2], // +1 for opening quote, +1 for closing
                    }
                }
            }
        }
        match args.find(',') {
            Some(p) => args[..p].trim(),
            None => args.trim(),
        }
    }

    /// Extracts the string literal from `USING MESSAGE = '<literal>'`.
    ///
    /// Returns `Ok(None)` when `content` does not begin with `USING`.
    /// Returns `Ok(Some(literal))` for the single-item `MESSAGE = '<literal>'`
    /// form. Returns [`Error::UnsupportedRaiseUsing`] for any other USING
    /// form (non-literal value, unrecognised item, or multiple items).
    fn extract_using_message_literal(content: &str) -> Result<Option<&str>, Error> {
        let trimmed = content.trim_start();
        if !trimmed
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("USING"))
        {
            return Ok(None);
        }

        let unsupported = || Error::UnsupportedRaiseUsing {
            clause: trimmed.to_string(),
        };
        let after_using = trimmed[5..].trim_start();
        if !after_using
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("MESSAGE"))
        {
            return Err(unsupported());
        }
        let after_message = after_using[7..].trim_start();
        let Some(rest) = after_message.strip_prefix('=') else {
            return Err(unsupported());
        };
        let rest = rest.trim_start();
        if !rest.starts_with('\'') {
            return Err(unsupported());
        }
        let literal = Self::extract_first_string_literal(rest);
        let remainder = rest[literal.len()..].trim_start();
        if !remainder.is_empty() {
            return Err(unsupported());
        }
        Ok(Some(literal))
    }

    /// Rewrites PL/pgSQL's `ELSIF` to the `ELSEIF` SQLite accepts, in live
    /// code only.
    ///
    /// Delegates to the shared scanner rather than tracking quotes itself.
    /// The hand-rolled version this replaces knew only `'` and `"`, so it
    /// rewrote inside a dollar-quoted literal, and it checked only the byte
    /// after the keyword, so a column named `preelsif` came out `preELSEIF`.
    /// `find_keyword` requires a word boundary on both sides and treats every
    /// quoted span, dollar quotes included, as untouchable.
    fn transform_elsif(body: &str) -> String {
        const KEYWORD: &str = "ELSIF";
        let scanner = Scanner::new(body);
        let mut result = String::with_capacity(body.len());
        let mut from = 0;
        while let Some(offset) = scanner.find_keyword(KEYWORD, from) {
            result.push_str(&body[from..offset]);
            result.push_str("ELSEIF");
            from = offset + KEYWORD.len();
        }
        result.push_str(&body[from..]);
        result
    }

    /// Transforms SELECT ... INTO var1, var2 FROM ... statements.
    ///
    /// PL/pgSQL: SELECT col1, col2 INTO v1, v2 FROM table WHERE ...
    /// We convert this to a form that records the bindings and removes the INTO
    /// clause since `SQLite` doesn't support SELECT INTO in triggers.
    fn transform_select_into(body: &str, context: &PlPgSqlContext) -> String {
        // We need to find SELECT ... INTO ... FROM patterns
        // and transform them into SET statements that can be parsed
        let mut result = String::new();
        let chars = body.chars();
        let mut current_stmt = String::new();
        let mut in_string = false;
        let mut string_char = ' ';

        for c in chars {
            // Track string literals
            if (c == '\'' || c == '"') && !in_string {
                in_string = true;
                string_char = c;
            } else if c == string_char && in_string {
                in_string = false;
            }

            current_stmt.push(c);

            // Check for statement end (semicolon not in string)
            if c == ';' && !in_string {
                // Process the statement
                let transformed = Self::try_transform_select_into_stmt(&current_stmt, context);
                result.push_str(&transformed);
                current_stmt.clear();
            }
        }

        // Handle any remaining content
        if !current_stmt.is_empty() {
            result.push_str(&current_stmt);
        }

        result
    }

    /// Tries to transform a single SELECT INTO statement.
    ///
    /// Handled shapes, each with an optional leading `WITH` clause that moves
    /// inside the generated subquery, since `SET` has no place for it:
    ///
    /// - `SELECT <exprs> INTO <vars> FROM ...` becomes one `SET var = (SELECT
    ///   expr FROM ... LIMIT 1);` per variable.
    /// - `SELECT <exprs> INTO <vars>;` with no FROM clause, the plpgsql
    ///   spelling of `var := expr`, becomes `SET var = (SELECT expr LIMIT 1);`.
    ///
    /// Keywords are located at the top nesting level only, outside strings and
    /// parentheses, so a CTE body's own SELECT or FROM never splits the
    /// statement. Anything left untransformed keeps its INTO and is refused
    /// loudly by the trigger body translator rather than emitted.
    fn try_transform_select_into_stmt(stmt: &str, _context: &PlPgSqlContext) -> String {
        // The main SELECT is the first top-level one: CTE bodies sit inside
        // parentheses, so a WITH prefix is skipped naturally.
        let Some(select_pos) = Self::find_top_level_keyword(stmt, "SELECT", 0) else {
            return stmt.to_string();
        };
        let Some(into_pos) = Self::find_top_level_keyword(stmt, "INTO", select_pos) else {
            return stmt.to_string();
        };
        let from_pos = Self::find_top_level_keyword(stmt, "FROM", into_pos);

        // A WITH prefix belongs inside each generated subquery.
        // MUTANT: equivalent: `<=` cannot differ, since WITH and SELECT are
        // different keywords and never share an offset.
        let with_pos = Self::find_top_level_keyword(stmt, "WITH", 0).filter(|w| *w < select_pos);
        let prefix = &stmt[..with_pos.unwrap_or(select_pos)];
        let with_part = with_pos.map_or("", |w| &stmt[w..select_pos]);

        let columns_part = stmt[select_pos + "SELECT".len()..into_pos].trim();
        let vars_end = from_pos.unwrap_or_else(|| stmt.rfind(';').unwrap_or(stmt.len()));
        let vars_part = stmt[into_pos + "INTO".len()..vars_end].trim();
        let from_part = from_pos.map_or("", |f| stmt[f..].trim_end());

        let columns = Self::split_top_level_csv(columns_part);
        let vars = Self::split_top_level_csv(vars_part);

        // A variable slot that is not a bare name (INTO STRICT var, a record
        // field, ...) has no SET spelling here. Left as-is, the INTO reaches
        // the trigger body translator, which refuses it naming the statement.
        if columns.len() != vars.len()
            || columns.is_empty()
            || vars.iter().any(|var| var.chars().any(char::is_whitespace))
        {
            return stmt.to_string();
        }

        let mut result = String::new();
        result.push_str(prefix);

        // Get indentation from the position of WITH or SELECT.
        let stmt_start = with_pos.unwrap_or(select_pos);
        let line_start = stmt[..stmt_start].rfind('\n').map_or(0, |p| p + 1);
        let indent = &stmt[line_start..stmt_start];

        for (i, (col, var)) in columns.iter().zip(vars.iter()).enumerate() {
            let body = format!(
                "{with_part}SELECT {col} {}",
                from_part.trim_end_matches(';')
            );
            // Add LIMIT 1 if not present. PostgreSQL's SELECT INTO takes the
            // first row, which a SQLite scalar subquery does anyway, so a
            // LIMIT hiding inside a CTE body suppressing this one is harmless.
            let subquery = if body.to_uppercase().contains(" LIMIT ") {
                format!("({})", body.trim_end())
            } else {
                format!("({} LIMIT 1)", body.trim_end())
            };

            if i > 0 {
                result.push_str(indent);
            }
            let _ = write!(result, "SET {var} = {subquery};");
            if i < columns.len() - 1 {
                result.push('\n');
            }
        }

        result
    }

    /// Splits a comma-separated SQL fragment on top-level commas only.
    ///
    /// Commas inside parentheses or quoted strings are ignored.
    fn split_top_level_csv(input: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut start = 0usize;
        let mut paren_depth = 0usize;
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut chars = input.char_indices().peekable();

        while let Some((idx, ch)) = chars.next() {
            if in_single_quote {
                if ch == '\'' {
                    if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                        // Escaped quote in SQL string literal ('')
                        chars.next();
                    } else {
                        in_single_quote = false;
                    }
                }
                continue;
            }

            if in_double_quote {
                if ch == '"' {
                    in_double_quote = false;
                }
                continue;
            }

            match ch {
                '\'' => in_single_quote = true,
                '"' => in_double_quote = true,
                '(' => paren_depth += 1,
                ')' => paren_depth = paren_depth.saturating_sub(1),
                ',' if paren_depth == 0 => {
                    let part = input[start..idx].trim();
                    if !part.is_empty() {
                        parts.push(part);
                    }
                    start = idx + 1;
                }
                _ => {}
            }
        }

        let tail = input[start..].trim();
        if !tail.is_empty() {
            parts.push(tail);
        }

        parts
    }

    /// Finds the first top-level occurrence of `keyword` at or after `start`.
    ///
    /// Top-level means outside string literals and at parenthesis depth zero,
    /// and the match must be delimited by non-word characters, so `FROM` inside
    /// a CTE body or a column named `INTOX` never matches. ASCII case
    /// insensitive. Comments are not tracked, matching the rest of this
    /// preprocessor.
    fn find_top_level_keyword(stmt: &str, keyword: &str, start: usize) -> Option<usize> {
        let bytes = stmt.as_bytes();
        let mut paren_depth = 0usize;
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut chars = stmt.char_indices().peekable();

        while let Some((idx, ch)) = chars.next() {
            if in_single_quote {
                if ch == '\'' {
                    if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                        chars.next();
                    } else {
                        in_single_quote = false;
                    }
                }
                continue;
            }
            if in_double_quote {
                if ch == '"' {
                    in_double_quote = false;
                }
                continue;
            }
            match ch {
                '\'' => in_single_quote = true,
                '"' => in_double_quote = true,
                '(' => paren_depth += 1,
                ')' => paren_depth = paren_depth.saturating_sub(1),
                _ => {
                    if paren_depth == 0
                        && idx >= start
                        && bytes
                            .get(idx..idx + keyword.len())
                            .is_some_and(|b| b.eq_ignore_ascii_case(keyword.as_bytes()))
                        && !Self::is_word_byte(bytes.get(idx.wrapping_sub(1)).copied(), idx == 0)
                        && !Self::is_word_byte(bytes.get(idx + keyword.len()).copied(), false)
                    {
                        return Some(idx);
                    }
                }
            }
        }
        None
    }

    /// True when `byte` continues an identifier. `at_boundary` marks the string
    /// edges, which always delimit.
    fn is_word_byte(byte: Option<u8>, at_boundary: bool) -> bool {
        if at_boundary {
            return false;
        }
        byte.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
    }

    /// Transforms variable assignments from := to SET syntax.
    ///
    /// Split on unquoted semicolons rather than newlines, since the right-hand
    /// side of an assignment can span lines and a statement ends at a
    /// semicolon.
    ///
    /// # Errors
    ///
    /// Returns [`Error::QualifiedAssignment`] when a statement assigns to
    /// a qualified target.
    fn transform_assignments(body: &str, context: &PlPgSqlContext) -> Result<String, Error> {
        let mut result = String::new();
        let mut start = 0;

        while let Some(offset) = Scanner::new(&body[start..]).find_in_code(b';') {
            let end = start + offset;
            reject_qualified_assignment(&body[start..end])?;
            result.push_str(&Self::rewrite_assignment(&body[start..end], context));
            result.push(';');
            start = end + 1;
        }
        reject_qualified_assignment(&body[start..])?;
        result.push_str(&Self::rewrite_assignment(&body[start..], context));
        Ok(result)
    }
    /// One statement, rewritten to `SET name = expr` when it is an assignment
    /// and returned unchanged when it is not.
    ///
    /// A chunk can carry text before the assignment, `BEGIN` on the first one
    /// and `END IF` after a branch, so the variable is the identifier
    /// immediately before `:=` and everything ahead of it passes through.
    fn rewrite_assignment(statement: &str, context: &PlPgSqlContext) -> String {
        let Some(operator) = Scanner::new(statement).find_str_in_code(":=") else {
            return statement.to_string();
        };
        let before = statement[..operator].trim_end();
        let (prefix, name) = before.split_at(trailing_identifier_start(before));

        let candidate = format!("{} := {}", name.trim(), statement[operator + 2..].trim());
        match Self::parse_assignment_line(&candidate, context) {
            Some(assignment) => {
                format!(
                    "{prefix}SET {} = {}",
                    assignment.name, assignment.expression
                )
            }
            None => statement.to_string(),
        }
    }

    /// Parses a line to see if it's a variable assignment.
    fn parse_assignment_line(line: &str, _context: &PlPgSqlContext) -> Option<VariableBinding> {
        // Remove trailing semicolon
        let line = line.trim().trim_end_matches(';').trim();

        // Look for :=
        let assign_pos = line.find(":=")?;

        let var_name = line[..assign_pos].trim();
        let expression = line[assign_pos + 2..].trim();

        // A name is valid when it could be an identifier at all.
        let is_valid_var = var_name.chars().all(|c| c.is_alphanumeric() || c == '_')
            && var_name
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_');

        if is_valid_var && !expression.is_empty() {
            Some(VariableBinding {
                name: var_name.to_string(),
                expression: expression.to_string(),
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn assignments_are_split_on_statements_not_lines() {
        let context = PlPgSqlContext::new();
        let body = "BEGIN\n    v_label :=\n        'a; b';\n    RETURN NEW;\nEND";
        let out = PlPgSqlPreprocessor::transform_assignments(body, &context).expect("no target");
        assert!(out.contains("SET v_label = 'a; b'"), "got: {out}");
    }

    #[test]
    fn test_split_declare_and_body() {
        let body = r"
DECLARE
    v_id UUID;
    v_name TEXT;
BEGIN
    v_id := uuidv7();
    INSERT INTO t (id) VALUES (v_id);
END
";

        let (declare, body) = PlPgSqlPreprocessor::split_declare_and_body(body);
        assert!(declare.is_some());
        assert!(declare.unwrap().contains("v_id UUID"));
        assert!(body.contains("BEGIN"));
    }

    #[test]
    fn test_parse_declaration() {
        let decl = PlPgSqlPreprocessor::parse_single_declaration("v_new_id UUID");
        assert!(decl.is_some());
        let decl = decl.unwrap();
        assert_eq!(decl.name, "v_new_id");
        assert_eq!(decl.data_type, "UUID");
        assert!(decl.default_value.is_none());
    }

    #[test]
    fn test_parse_declaration_with_default() {
        let decl = PlPgSqlPreprocessor::parse_single_declaration("v_count INT DEFAULT 0");
        assert!(decl.is_some());
        let decl = decl.unwrap();
        assert_eq!(decl.name, "v_count");
        assert_eq!(decl.data_type, "INT");
        assert_eq!(decl.default_value, Some("0".to_string()));
    }

    #[test]
    fn test_transform_select_into() {
        let body = r"BEGIN
    SELECT o.owner_id, o.creator_id, e.role_level
    INTO v_owner_id, v_creator_id, v_role_level
    FROM ownables o
    JOIN entities e ON o.id = e.id
    WHERE o.id = NEW.id;
END";

        let (transformed, _context) = PlPgSqlPreprocessor::preprocess(body).expect("preprocess");

        // Should have SET statements instead of SELECT INTO
        assert!(
            transformed.contains("SET v_owner_id ="),
            "Should transform v_owner_id"
        );
        assert!(
            transformed.contains("SET v_creator_id ="),
            "Should transform v_creator_id"
        );
        assert!(
            transformed.contains("SET v_role_level ="),
            "Should transform v_role_level"
        );
        assert!(
            !transformed.contains("INTO v_owner_id"),
            "Should remove INTO clause"
        );
    }

    #[test]
    fn test_transform_select_into_with_declare() {
        let body = r"DECLARE
    v_new_id UUID;
    v_owner_id UUID;
BEGIN
    SELECT o.owner_id, o.creator_id
    INTO v_owner_id, v_creator_id
    FROM ownables o
    WHERE o.id = NEW.id;
END";

        let (transformed, _context) = PlPgSqlPreprocessor::preprocess(body).expect("preprocess");

        // Should have SET statements instead of SELECT INTO
        assert!(
            transformed.contains("SET v_owner_id ="),
            "Should transform v_owner_id"
        );
        assert!(
            transformed.contains("SET v_creator_id ="),
            "Should transform v_creator_id"
        );
        assert!(
            !transformed.contains("INTO v_owner_id"),
            "Should remove INTO clause"
        );
    }

    #[test]
    fn test_transform_select_into_with_comma_in_expression() {
        let body = r"BEGIN
    SELECT COALESCE(NEW.a, NEW.b)
    INTO v_value
    FROM t
    LIMIT 1;
END";

        let (transformed, _context) = PlPgSqlPreprocessor::preprocess(body).expect("preprocess");

        assert!(
            transformed.contains("SET v_value ="),
            "Should transform INTO target"
        );
        assert!(
            transformed.contains("COALESCE(NEW.a, NEW.b)"),
            "Should preserve expression with comma"
        );
        assert!(!transformed.contains(" INTO "), "Should remove INTO clause");
    }

    #[test]
    fn test_parse_declaration_and_assignment_edge_cases() {
        assert!(PlPgSqlPreprocessor::parse_single_declaration("").is_none());
        assert!(PlPgSqlPreprocessor::parse_single_declaration("only_name").is_none());

        let decl = PlPgSqlPreprocessor::parse_single_declaration("v_now TIMESTAMP := now()");
        assert!(decl.is_some());
        let decl = decl.unwrap();
        assert_eq!(decl.name, "v_now");
        assert_eq!(decl.data_type, "TIMESTAMP");
        assert_eq!(decl.default_value.as_deref(), Some("now()"));

        let context = PlPgSqlContext::new();
        assert!(PlPgSqlPreprocessor::parse_assignment_line("1bad := 1;", &context).is_none());
        assert!(PlPgSqlPreprocessor::parse_assignment_line("v_ok := ;", &context).is_none());
    }

    #[test]
    fn test_transform_elsif_handles_partial_and_identifier_suffix_inputs() {
        let single = PlPgSqlPreprocessor::transform_elsif("E");
        assert_eq!(single, "E");

        let transformed = PlPgSqlPreprocessor::transform_elsif("E\nELS\nELSIFX\nELSIF\n");
        assert!(transformed.contains("E\n"));
        assert!(transformed.contains("ELS\n"));
        assert!(transformed.contains("ELSIFX\n"));
        assert!(transformed.contains("ELSEIF\n"));
    }

    #[test]
    fn test_transform_raise_exception_simple() {
        let body = "BEGIN\n  RAISE EXCEPTION 'val must be non-negative';\n  RETURN NEW;\nEND";
        let (transformed, _) = PlPgSqlPreprocessor::preprocess(body).expect("preprocess");
        assert!(
            transformed.contains("SELECT RAISE(ABORT, 'val must be non-negative')"),
            "RAISE EXCEPTION should become SELECT RAISE(ABORT, ...), got: {transformed}"
        );
        assert!(
            !transformed.contains("RAISE EXCEPTION"),
            "Original form should be removed"
        );
    }

    /// The `SELECT RAISE(ABORT, ...)` message for the RAISE EXCEPTION
    /// statement `raise`.
    fn raised_message(raise: &str) -> String {
        let body = format!("BEGIN\n  {raise};\n  RETURN NEW;\nEND");
        let (transformed, _) = PlPgSqlPreprocessor::preprocess(&body).expect("preprocess");
        let start = transformed
            .find("SELECT RAISE(ABORT, ")
            .expect("the RAISE is rewritten")
            + "SELECT RAISE(ABORT, ".len();
        let end = transformed[start..].find(");").expect("RAISE closes") + start;
        transformed[start..end].to_string()
    }

    #[test]
    fn each_placeholder_takes_the_next_argument() {
        assert_eq!(
            raised_message("RAISE EXCEPTION 'bad value: %', NEW.val"),
            "'bad value: ' || coalesce(NEW.val, '<NULL>')"
        );
        assert_eq!(
            raised_message("RAISE EXCEPTION '% of %: %', coalesce(a, 1), b, 'x, y'"),
            "coalesce(coalesce(a, 1), '<NULL>') || ' of ' || coalesce(b, '<NULL>') \
             || ': ' || coalesce('x, y', '<NULL>')"
        );
    }

    #[test]
    fn a_doubled_percent_is_one_literal_percent() {
        assert_eq!(raised_message("RAISE EXCEPTION 'a %% b'"), "'a % b'");
        assert_eq!(
            raised_message("RAISE EXCEPTION '%% of %', x"),
            "'% of ' || coalesce(x, '<NULL>')"
        );
    }

    #[test]
    fn a_quote_in_the_format_stays_escaped() {
        assert_eq!(
            raised_message("RAISE EXCEPTION 'it''s %', x"),
            "'it''s ' || coalesce(x, '<NULL>')"
        );
    }

    #[test]
    fn using_options_after_the_arguments_are_not_arguments() {
        assert_eq!(
            raised_message("RAISE EXCEPTION 'v=%', x USING HINT = 'h, i'"),
            "'v=' || coalesce(x, '<NULL>')"
        );
    }

    #[test]
    fn a_placeholder_count_postgresql_refuses_is_refused() {
        for (raise, expected_placeholders, expected_arguments) in [
            ("RAISE EXCEPTION 'a % b'", 1, 0),
            ("RAISE EXCEPTION 'a % b %', x", 2, 1),
            ("RAISE EXCEPTION 'a %', x, y", 1, 2),
            ("RAISE EXCEPTION 'a %%', x", 0, 1),
        ] {
            let body = format!("BEGIN\n  {raise};\nEND");
            let result = PlPgSqlPreprocessor::preprocess(&body);
            assert!(
                matches!(
                    result,
                    Err(Error::RaiseParameterCount { placeholders, arguments, .. })
                        if (placeholders, arguments) == (expected_placeholders, expected_arguments)
                ),
                "{raise}: {result:?}"
            );
        }
    }

    #[test]
    fn test_transform_raise_notice_dropped() {
        let body = "BEGIN\n  RAISE NOTICE 'debug info';\n  RETURN NEW;\nEND";
        let (transformed, _) = PlPgSqlPreprocessor::preprocess(body).expect("preprocess");
        assert!(
            !transformed.contains("RAISE NOTICE"),
            "RAISE NOTICE should be dropped, got: {transformed}"
        );
    }

    #[test]
    fn an_exception_handler_is_detected() {
        let body = "BEGIN\n  SELECT 1;\nEXCEPTION WHEN others THEN\n  NULL;\nEND";
        assert!(PlPgSqlPreprocessor::has_exception_handler(body));
        assert!(!PlPgSqlPreprocessor::has_exception_handler(
            "BEGIN\n  RAISE EXCEPTION 'boom';\nEND"
        ));
    }

    #[test]
    fn a_top_level_keyword_ignores_quoted_and_parenthesized_text() {
        let find = PlPgSqlPreprocessor::find_top_level_keyword;
        assert_eq!(
            find("SELECT 'INTO x' FROM t", "INTO", 0),
            None,
            "single quoted"
        );
        assert_eq!(find("SELECT \"INTO\" x", "INTO", 0), None, "double quoted");
        assert_eq!(find("SELECT (INTO) x", "INTO", 0), None, "parenthesized");
    }

    #[test]
    fn a_quote_closes_and_the_keyword_after_it_is_found() {
        let find = PlPgSqlPreprocessor::find_top_level_keyword;
        assert_eq!(find("SELECT 'a' INTO x", "INTO", 0), Some(11));
        assert_eq!(find("SELECT \"a\" INTO x", "INTO", 0), Some(11));
    }

    /// A keyword glued to an identifier is part of that identifier.
    #[test]
    fn a_top_level_keyword_needs_a_word_boundary() {
        let find = PlPgSqlPreprocessor::find_top_level_keyword;
        assert_eq!(find("xINTO y", "INTO", 0), None);
        assert_eq!(find("INTO y", "INTO", 0), Some(0), "at the very start");
    }

    #[test]
    fn a_qualified_assignment_is_refused() {
        assert!(matches!(
            PlPgSqlPreprocessor::preprocess("BEGIN\n  NEW.col := 1;\nEND").unwrap_err(),
            Error::QualifiedAssignment { .. }
        ));
    }

    #[test]
    fn a_qualifier_without_a_name_is_not_a_qualified_target() {
        for body in ["BEGIN\n  rec. := 1;\nEND", "BEGIN\n  rec. = 1;\nEND"] {
            let (out, _) = PlPgSqlPreprocessor::preprocess(body).expect("no qualified target");
            assert_eq!(out, body);
        }
    }

    fn declared_default(body: &str) -> Option<String> {
        let (_, ctx) = PlPgSqlPreprocessor::preprocess(body).expect("preprocess");
        ctx.get_declaration("v")
            .and_then(|d| d.default_value.clone())
    }

    #[test]
    fn a_dollar_quoted_default_becomes_single_quoted() {
        let untagged =
            declared_default("DECLARE\n  v TEXT := $$hello$$;\nBEGIN\n  RETURN NEW;\nEND");
        assert_eq!(untagged.as_deref(), Some("'hello'"));

        let tagged =
            declared_default("DECLARE\n  v TEXT := $t_g$hello$t_g$;\nBEGIN\n  RETURN NEW;\nEND");
        assert_eq!(tagged.as_deref(), Some("'hello'"), "an underscore tag");
    }

    #[test]
    fn a_plain_default_passes_through_unchanged() {
        let plain = declared_default("DECLARE\n  v INT := 42;\nBEGIN\n  RETURN NEW;\nEND");
        assert_eq!(plain.as_deref(), Some("42"));
    }

    #[test]
    fn raise_followed_by_a_multibyte_character_is_passed_through() {
        let body = "BEGIN\n  RAISE\u{e9};\nEND";
        let (transformed, _) = PlPgSqlPreprocessor::preprocess(body).expect("preprocess");
        assert!(transformed.contains("RAISE\u{e9}"));
    }

    /// A level word is matched without case folding the whole body, so an
    /// offset can never point into a differently sized uppercase form.
    #[test]
    fn raise_level_matching_ignores_case() {
        let body = "BEGIN\n  raise notice 'x';\n  RETURN NEW;\nEND";
        let (transformed, _) = PlPgSqlPreprocessor::preprocess(body).expect("preprocess");
        assert!(!transformed.to_uppercase().contains("RAISE NOTICE"));
    }

    #[test]
    fn raise_exception_without_a_semicolon_keeps_the_whole_message() {
        let body = "BEGIN\n  RAISE EXCEPTION 'boom'";
        let (transformed, _) = PlPgSqlPreprocessor::preprocess(body).expect("preprocess");
        assert!(
            transformed.contains("'boom'"),
            "the closing quote must survive, got: {transformed}"
        );
    }

    /// A message cut off right after its opening quote has no format text to
    /// scan, and passes through as found.
    #[test]
    fn a_lone_quote_as_the_raise_message_passes_through() {
        let (transformed, _) =
            PlPgSqlPreprocessor::preprocess("BEGIN\n  RAISE EXCEPTION '").expect("preprocess");
        assert!(
            transformed.contains("SELECT RAISE(ABORT, ');"),
            "got: {transformed}"
        );
    }

    #[test]
    fn test_split_top_level_csv_handles_escaped_single_and_double_quotes() {
        let parts = PlPgSqlPreprocessor::split_top_level_csv(
            "'a''b', \"x,y\", COALESCE(a, b), plain_value",
        );
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "'a''b'");
        assert_eq!(parts[1], "\"x,y\"");
        assert_eq!(parts[2], "COALESCE(a, b)");
        assert_eq!(parts[3], "plain_value");
    }

    proptest! {
        #[test]
        fn preprocess_never_panics(body in ".*") {
            let _ = PlPgSqlPreprocessor::preprocess(&body);
        }
    }

    #[test]
    fn elsif_inside_a_string_is_not_rewritten() {
        let body = "BEGIN\n  x := 'ELSIF goes here';\nEND";
        let (out, _) = PlPgSqlPreprocessor::preprocess(body).unwrap();
        assert!(
            out.contains("'ELSIF goes here'"),
            "literal unchanged: {out}"
        );
        assert!(
            !out.contains("'ELSEIF"),
            "must not rewrite inside string: {out}"
        );
    }

    #[test]
    fn exception_in_string_is_not_a_handler() {
        assert!(!PlPgSqlPreprocessor::has_exception_handler(
            "BEGIN\n  x := 'EXCEPTION WHEN';\nEND"
        ));
    }

    fn preprocessed(body: &str) -> String {
        PlPgSqlPreprocessor::preprocess(body).expect("preprocess").0
    }

    /// A `=` inside an open CASE is a comparison, not an assignment target.
    #[test]
    fn a_qualified_equals_inside_a_case_is_a_comparison() {
        let body = "BEGIN\n  v := CASE WHEN a THEN t.n = 1 ELSE FALSE END;\nEND";
        assert!(PlPgSqlPreprocessor::preprocess(body).is_ok());
    }

    #[test]
    fn a_qualified_equals_after_then_is_refused() {
        assert!(matches!(
            PlPgSqlPreprocessor::preprocess("BEGIN\n  IF a THEN NEW.col = 1;\n  END IF;\nEND")
                .unwrap_err(),
            Error::QualifiedAssignment { .. }
        ));
    }

    #[test]
    fn a_qualified_comparison_operator_is_not_an_assignment() {
        assert!(PlPgSqlPreprocessor::preprocess("BEGIN\n  IF NEW.col == 1 THEN x;\nEND").is_ok());
        assert!(PlPgSqlPreprocessor::preprocess("BEGIN\n  IF NEW.col => 1 THEN x;\nEND").is_ok());
    }

    #[test]
    fn every_informational_raise_level_is_dropped() {
        for body in [
            "BEGIN\n  RAISE INFO 'x';\n  RETURN NEW;\nEND",
            "BEGIN\n  RAISE INFO\n    'x';\n  RETURN NEW;\nEND",
            "BEGIN\n  RAISE INFO;\n  RETURN NEW;\nEND",
            "BEGIN\n  RAISE DEBUG 'x';\n  RETURN NEW;\nEND",
            "BEGIN\n  RAISE LOG 'x';\n  RETURN NEW;\nEND",
            "BEGIN\n  RAISE WARNING 'x';\n  RETURN NEW;\nEND",
        ] {
            let out = preprocessed(body);
            assert!(!out.to_uppercase().contains("RAISE"), "not dropped: {out}");
            assert!(out.contains("RETURN NEW"), "lost the tail: {out}");
        }
    }

    /// Dropping a RAISE consumes its terminating semicolon and nothing else.
    #[test]
    fn dropping_a_raise_consumes_exactly_its_semicolon() {
        let out = preprocessed("BEGIN\n  RAISE NOTICE 'x';\n  RETURN NEW;\nEND");
        assert_eq!(out.matches(';').count(), 1, "stray semicolon: {out}");
    }

    proptest! {
        #[test]
        fn exception_handler_detection_never_panics(s in ".*") {
            let _ = PlPgSqlPreprocessor::has_exception_handler(&s);
        }
    }
    #[test]
    fn select_into_becomes_one_set_per_variable() {
        assert_eq!(
            preprocessed("BEGIN\n  SELECT a, b INTO x, y FROM t;\nEND"),
            "BEGIN\n  SET x = (SELECT a FROM t LIMIT 1);\n  SET y = (SELECT b FROM t LIMIT 1);\nEND"
        );
    }

    /// A leading CTE belongs inside every generated subquery.
    #[test]
    fn a_with_prefix_moves_into_the_subquery() {
        assert_eq!(
            preprocessed("BEGIN\n  WITH c AS (SELECT 1) SELECT a INTO x FROM c;\nEND"),
            "BEGIN\n  SET x = (WITH c AS (SELECT 1) SELECT a FROM c LIMIT 1);\nEND"
        );
    }

    /// Shapes with no SET spelling are left for the body translator to refuse.
    #[test]
    fn select_into_is_left_alone_when_it_cannot_be_rewritten() {
        for body in [
            "BEGIN\n  SELECT a, b INTO v FROM t;\nEND",
            "BEGIN\n  SELECT a INTO STRICT v FROM t;\nEND",
        ] {
            assert_eq!(preprocessed(body), body, "should pass through");
        }
    }

    #[test]
    fn a_qualified_comparison_operator_at_a_statement_start_is_not_an_assignment() {
        assert!(PlPgSqlPreprocessor::preprocess("BEGIN\n  NEW.col == 1;\nEND").is_ok());
        assert!(PlPgSqlPreprocessor::preprocess("BEGIN\n  NEW.col => 1;\nEND").is_ok());
    }
    #[test]
    fn declare_is_only_a_block_when_it_precedes_begin() {
        let (decls, body) =
            PlPgSqlPreprocessor::split_declare_and_body("DECLARE\n  v INT;\nBEGIN\n  x;\nEND");
        assert_eq!(decls.as_deref(), Some("v INT;"));
        assert_eq!(body, "BEGIN\n  x;\nEND");

        let (none, from_begin) =
            PlPgSqlPreprocessor::split_declare_and_body("\n  BEGIN\n  x;\nEND");
        assert!(none.is_none());
        assert_eq!(
            from_begin, "BEGIN\n  x;\nEND",
            "text before BEGIN is dropped"
        );

        let (after, whole) =
            PlPgSqlPreprocessor::split_declare_and_body("BEGIN\n  DECLARE x;\nEND");
        assert!(after.is_none(), "DECLARE after BEGIN is not a block");
        assert_eq!(whole, "BEGIN\n  DECLARE x;\nEND");
    }

    #[test]
    fn a_doubled_quote_stays_inside_a_raise_message() {
        let out = preprocessed("BEGIN\n  RAISE EXCEPTION 'it''s';\nEND");
        assert!(out.contains("'it''s'"), "literal was cut short: {out}");
    }

    #[test]
    fn raise_using_message_is_extracted_and_other_clauses_refused() {
        let out = preprocessed("BEGIN\n  RAISE EXCEPTION USING MESSAGE = 'boom';\nEND");
        assert!(out.contains("'boom'"), "message not used: {out}");

        assert!(matches!(
            PlPgSqlPreprocessor::preprocess("BEGIN\n  RAISE EXCEPTION USING DETAIL = 'x';\nEND")
                .unwrap_err(),
            Error::UnsupportedRaiseUsing { .. }
        ));
    }

    /// A semicolon or comma inside a literal must not split anything.
    #[test]
    fn punctuation_inside_a_literal_does_not_split() {
        assert_eq!(
            preprocessed("BEGIN\n  SELECT 'a;b' INTO x FROM t;\nEND"),
            "BEGIN\n  SET x = (SELECT 'a;b' FROM t LIMIT 1);\nEND"
        );
        assert_eq!(
            preprocessed("BEGIN\n  SELECT 'a,b' INTO x FROM t;\nEND"),
            "BEGIN\n  SET x = (SELECT 'a,b' FROM t LIMIT 1);\nEND"
        );
    }

    #[test]
    fn a_keyword_inside_a_quoted_span_is_never_top_level() {
        let find = PlPgSqlPreprocessor::find_top_level_keyword;
        assert_eq!(find("SELECT 'x INTO' FROM t", "INTO", 0), None);
        assert_eq!(find("SELECT \"x INTO\" FROM t", "INTO", 0), None);
    }
}
