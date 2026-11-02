//! Error type for PL/pgSQL preprocessing and parsing.

use alloc::string::String;

/// An error produced while preprocessing or parsing a PL/pgSQL function body.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The function contains an exception handler.
    #[error(
        "trigger function '{name}' uses an exception handler (EXCEPTION WHEN), \
         which has no generic equivalent"
    )]
    ExceptionHandler {
        /// The function name.
        name: String,
    },

    /// A qualified assignment target cannot be preprocessed.
    #[error(
        "assignment to qualified target '{qualifier}.{name}' cannot be \
         preprocessed as a standalone statement"
    )]
    QualifiedAssignment {
        /// The target qualifier.
        qualifier: String,
        /// The target name.
        name: String,
    },

    /// The RAISE EXCEPTION USING clause is unsupported.
    #[error(
        "RAISE EXCEPTION USING {clause}: only USING MESSAGE = '<string literal>' \
         can be preprocessed"
    )]
    UnsupportedRaiseUsing {
        /// The unsupported clause.
        clause: String,
    },

    /// Tokenizing the function body failed.
    #[error("trigger function '{name}' body could not be tokenized: {source}. Body: {body}")]
    Tokenization {
        /// The function name.
        name: String,
        /// The preprocessed body that failed.
        body: String,
        /// The tokenizer error.
        #[source]
        source: sqlparser::tokenizer::TokenizerError,
    },

    /// The function body has no BEGIN...END block.
    #[error("trigger function '{name}' body has no BEGIN...END block. Body: {body}")]
    MissingBeginBlock {
        /// The function name.
        name: String,
        /// The preprocessed body that failed.
        body: String,
    },

    /// The function body has no END statement.
    #[error("trigger function '{name}' body has no END. Body: {body}")]
    MissingEndBlock {
        /// The function name.
        name: String,
        /// The preprocessed body that failed.
        body: String,
    },

    /// The function body opens a dollar-quoted literal that never closes.
    #[error("trigger function '{name}' body has an unterminated dollar-quoted string")]
    UnterminatedDollarQuote {
        /// The function name.
        name: String,
    },

    /// Parsing the function body statements failed.
    #[error(
        "trigger function '{name}' body statements could not be parsed: {source}. Body: {body}"
    )]
    ParseStatements {
        /// The function name.
        name: String,
        /// The preprocessed body that failed.
        body: String,
        /// The parser error.
        #[source]
        source: sqlparser::parser::ParserError,
    },

    /// A PL/pgSQL body read for its embedded SQL could not be tokenized.
    #[error("PL/pgSQL body could not be tokenized: {source}")]
    UntokenizableBody {
        /// The tokenizer error.
        #[source]
        source: sqlparser::tokenizer::TokenizerError,
    },

    /// A PL/pgSQL body does not have the shape PL/pgSQL requires.
    #[error("PL/pgSQL body expected {expected}, found {found}")]
    Syntax {
        /// What the grammar requires at this point.
        expected: &'static str,
        /// The token found instead.
        found: String,
    },

    /// An SQL statement or expression a PL/pgSQL body embeds could not be
    /// parsed.
    #[error("embedded SQL `{sql}` could not be parsed: {source}")]
    EmbeddedSql {
        /// The SQL as the parser saw it, one space between tokens.
        sql: String,
        /// The parser error.
        #[source]
        source: sqlparser::parser::ParserError,
    },

    /// `OPEN` names a cursor that no enclosing block binds to a query.
    #[error("cursor '{name}' is not bound to a query by any enclosing block")]
    UnboundCursor {
        /// The cursor name, folded as PostgreSQL folds it.
        name: String,
    },

    /// A PL/pgSQL body nests blocks and control structures deeper than the
    /// reader follows.
    #[error("PL/pgSQL body nests blocks and control structures deeper than {limit}")]
    NestingTooDeep {
        /// The deepest nesting followed.
        limit: usize,
    },
}
