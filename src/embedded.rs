//! Reading the SQL a PL/pgSQL function body runs, as a tree of the body's
//! blocks, branches, loops and exception handlers.
//!
//! PostgreSQL evaluates every PL/pgSQL expression as a `SELECT`, so each one
//! appears here as the `SELECT` it runs. Names the body declares and the
//! function's parameters are left as written.

use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use core::ops::Range;

use sqlparser::{
    ast::Statement,
    dialect::PostgreSqlDialect,
    keywords::Keyword,
    parser::Parser,
    tokenizer::{Token, Tokenizer, Word},
};

use crate::{Region, Scanner};

/// How deep blocks and control structures may nest before reading stops,
/// which keeps a hostile body from exhausting the stack.
const NESTING_LIMIT: usize = 50;

/// An error produced while reading the SQL a PL/pgSQL body embeds.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The body could not be tokenized.
    #[error("PL/pgSQL body could not be tokenized: {source}")]
    UntokenizableBody {
        /// The tokenizer error.
        #[source]
        source: sqlparser::tokenizer::TokenizerError,
    },

    /// The body does not have the shape PL/pgSQL requires.
    #[error("PL/pgSQL body expected {expected}, found {found}")]
    Syntax {
        /// What the grammar requires at this point.
        expected: &'static str,
        /// The token found instead.
        found: String,
    },

    /// An SQL statement or expression the body embeds could not be parsed.
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

    /// The body nests blocks and control structures deeper than the reader
    /// follows.
    #[error("PL/pgSQL body nests blocks and control structures deeper than {limit}")]
    NestingTooDeep {
        /// The deepest nesting followed.
        limit: usize,
    },
}

/// A block, `[DECLARE ...] BEGIN ... [EXCEPTION ...] END`.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    /// The default of each variable that declares one, as the `SELECT`
    /// evaluated on entering the block.
    ///
    /// These run before the block's handlers take effect, so an error here
    /// reaches the enclosing block's handlers instead.
    pub declarations: Vec<Statement>,
    /// The block's statements.
    pub statements: Vec<Node>,
    /// The statements of each `EXCEPTION WHEN ... THEN` handler.
    ///
    /// A handler runs once a statement raises, after PostgreSQL has rolled back
    /// everything the block's statements did, `SET` included.
    pub handlers: Vec<Vec<Node>>,
}

/// One step of a body.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// An SQL statement or query PostgreSQL runs at this point, with any
    /// `INTO` target removed.
    Statement(Box<Statement>),
    /// SQL text the body builds at run time and executes.
    Dynamic(Dynamic),
    /// A nested block.
    Block(Block),
    /// An `IF` statement.
    If(If),
    /// A `CASE` statement.
    Case(Case),
    /// A loop of any kind.
    Loop(Loop),
}

/// SQL built at run time by `EXECUTE`, `RETURN QUERY EXECUTE`,
/// `OPEN ... FOR EXECUTE` or `FOR ... IN EXECUTE`, which no reader can
/// resolve.
#[derive(Clone, Debug, PartialEq)]
pub struct Dynamic {
    /// The `SELECT` that evaluates the command string.
    pub command: Box<Statement>,
    /// The `SELECT` that evaluates each `USING` argument.
    pub arguments: Vec<Statement>,
}

/// A condition and the statements that run when it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Branch {
    /// The `SELECT` that evaluates the condition.
    ///
    /// For a `WHEN` of a `CASE` with a subject, it selects the listed values
    /// the subject is compared with.
    pub condition: Box<Statement>,
    /// The statements of the branch.
    pub statements: Vec<Node>,
}

/// `IF ... THEN ... [ELSIF ... THEN ...] [ELSE ...] END IF`.
#[derive(Clone, Debug, PartialEq)]
pub struct If {
    /// The `IF` branch and every `ELSIF`, tried in order until one holds.
    pub branches: Vec<Branch>,
    /// The `ELSE` statements.
    pub otherwise: Vec<Node>,
}

/// `CASE [subject] WHEN ... THEN ... [ELSE ...] END CASE`.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    /// The `SELECT` that evaluates the subject, when there is one.
    pub subject: Option<Box<Statement>>,
    /// Every `WHEN`, tried in order until one holds.
    pub branches: Vec<Branch>,
    /// The `ELSE` statements.
    pub otherwise: Vec<Node>,
}

/// A loop and the statements it repeats.
#[derive(Clone, Debug, PartialEq)]
pub struct Loop {
    /// What controls the loop.
    pub control: LoopControl,
    /// The statements each iteration runs.
    pub statements: Vec<Node>,
}

/// The SQL that controls a loop, all of which runs before the first
/// iteration.
#[derive(Clone, Debug, PartialEq)]
pub enum LoopControl {
    /// `LOOP`, which only `EXIT` or `RETURN` ends.
    Unconditional,
    /// `WHILE condition LOOP`, as the `SELECT` evaluated before each iteration.
    While(Box<Statement>),
    /// `FOR i IN [REVERSE] lower .. upper [BY step] LOOP`, each bound as the
    /// `SELECT` evaluated once.
    Range {
        /// The first bound written.
        lower: Box<Statement>,
        /// The second bound written.
        upper: Box<Statement>,
        /// The `BY` step.
        step: Option<Box<Statement>>,
    },
    /// `FOR target IN query LOOP`.
    Query(Box<Statement>),
    /// `FOR target IN cursor [(arguments)] LOOP`.
    Cursor {
        /// The `SELECT` that evaluates each argument.
        arguments: Vec<Statement>,
        /// The query the cursor's declaration binds.
        query: Box<Statement>,
    },
    /// `FOR target IN EXECUTE command [USING ...] LOOP`.
    Dynamic(Dynamic),
    /// `FOREACH target [SLICE n] IN ARRAY array LOOP`, as the `SELECT` that
    /// evaluates the array.
    Array(Box<Statement>),
}

/// Reads every SQL statement and expression a PL/pgSQL body runs.
///
/// `body` is the function's source between its dollar quotes. A cursor's
/// query appears where the cursor opens, since that is when PostgreSQL plans
/// it.
///
/// # Errors
///
/// Returns an [`Error`] when the body is not PL/pgSQL, when an SQL statement
/// or expression in it does not parse, when `OPEN` names a cursor no
/// enclosing block binds, or when it nests deeper than the reader follows.
///
/// # Example
///
/// ```
/// use sqlparser_plpgsql::{embedded::Node, embedded_sql};
///
/// let block = embedded_sql(
///     "BEGIN PERFORM 1 FROM docs; EXECUTE 'DELETE FROM ' || t; END",
/// )?;
/// let Node::Statement(perform) = &block.statements[0] else { unreachable!() };
/// assert_eq!(perform.to_string(), "SELECT 1 FROM docs");
/// assert!(matches!(block.statements[1], Node::Dynamic(_)));
/// # Ok::<(), sqlparser_plpgsql::embedded::Error>(())
/// ```
pub fn embedded_sql(body: &str) -> Result<Block, Error> {
    let text = spaced_ranges(body);
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, &text)
        .tokenize()
        .map_err(|source| Error::UntokenizableBody { source })?
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect();
    let mut reader = Reader {
        tokens,
        index: 0,
        depth: 0,
        scopes: Vec::new(),
    };

    reader.compiler_options()?;
    reader.label()?;
    let block = reader.nested(Reader::block)?;
    reader.eat_token(&Token::SemiColon);
    if reader.peek().is_some() {
        return Err(reader.syntax("the end of the body"));
    }
    Ok(block)
}

/// Sets every `..` of live code apart from its neighbours.
///
/// The tokenizer reads `1..10` as the numbers `1.` and `.10`, which would
/// hide the range of an integer `FOR` loop.
fn spaced_ranges(body: &str) -> Cow<'_, str> {
    let regions = Scanner::new(body).regions();
    let bytes = body.as_bytes();
    let live = |index: usize| regions.get(index) == Some(&Region::Code);
    let mut spaced = String::new();
    let mut copied = 0;
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'.' && bytes[index + 1] == b'.' && live(index) {
            spaced.push_str(&body[copied..index]);
            spaced.push_str(" .. ");
            index += 2;
            copied = index;
        } else {
            index += 1;
        }
    }
    if copied == 0 {
        return Cow::Borrowed(body);
    }
    spaced.push_str(&body[copied..]);
    Cow::Owned(spaced)
}

/// The PL/pgSQL statements recognised by their first word.
#[derive(Clone, Copy)]
enum Command {
    Block,
    If,
    Case,
    Loop,
    While,
    For,
    Foreach,
    Exit,
    Return,
    Raise,
    Assert,
    Execute,
    Perform,
    Open,
    Fetch,
    Silent,
}

impl Command {
    /// The statement `word` opens, when it is a PL/pgSQL keyword rather than
    /// the start of SQL or of an assignment.
    fn of(word: &Word) -> Option<Self> {
        const COMMANDS: [(&str, Command); 23] = [
            ("DECLARE", Command::Block),
            ("BEGIN", Command::Block),
            ("IF", Command::If),
            ("CASE", Command::Case),
            ("LOOP", Command::Loop),
            ("WHILE", Command::While),
            ("FOR", Command::For),
            ("FOREACH", Command::Foreach),
            ("EXIT", Command::Exit),
            ("CONTINUE", Command::Exit),
            ("RETURN", Command::Return),
            ("RAISE", Command::Raise),
            ("ASSERT", Command::Assert),
            ("EXECUTE", Command::Execute),
            ("PERFORM", Command::Perform),
            ("OPEN", Command::Open),
            ("FETCH", Command::Fetch),
            ("MOVE", Command::Fetch),
            ("GET", Command::Silent),
            ("CLOSE", Command::Silent),
            ("NULL", Command::Silent),
            ("COMMIT", Command::Silent),
            ("ROLLBACK", Command::Silent),
        ];
        if word.quote_style.is_some() {
            return None;
        }
        COMMANDS
            .iter()
            .find(|(name, _)| word.value.eq_ignore_ascii_case(name))
            .map(|&(_, command)| command)
    }
}

/// Where a run of SQL tokens ends.
#[derive(Clone, Copy)]
enum End {
    Semicolon,
    Loop,
}

impl End {
    fn at(self, token: &Token) -> bool {
        match self {
            End::Semicolon => *token == Token::SemiColon,
            End::Loop => is_keyword(token, "LOOP"),
        }
    }
}

/// A name a block declares, with the query it binds when it is a cursor.
struct Declared {
    name: String,
    cursor: Option<Box<Statement>>,
}

/// A recursive-descent walk over the body's tokens.
struct Reader {
    tokens: Vec<Token>,
    index: usize,
    depth: usize,
    scopes: Vec<Vec<Declared>>,
}

impl Reader {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn peek_nth(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.index + offset)
    }

    fn at(&self, keyword: &str) -> bool {
        self.peek().is_some_and(|token| is_keyword(token, keyword))
    }

    fn at_any(&self, keywords: &[&str]) -> bool {
        keywords.iter().any(|keyword| self.at(keyword))
    }

    fn eat(&mut self, keyword: &str) -> bool {
        let found = self.at(keyword);
        if found {
            self.index += 1;
        }
        found
    }

    fn eat_token(&mut self, token: &Token) -> bool {
        let found = self.peek() == Some(token);
        if found {
            self.index += 1;
        }
        found
    }

    fn expect(&mut self, keyword: &'static str) -> Result<(), Error> {
        if self.eat(keyword) {
            Ok(())
        } else {
            Err(self.syntax(keyword))
        }
    }

    fn expect_token(&mut self, token: &Token, expected: &'static str) -> Result<(), Error> {
        if self.eat_token(token) {
            Ok(())
        } else {
            Err(self.syntax(expected))
        }
    }

    fn expect_semicolon(&mut self) -> Result<(), Error> {
        self.expect_token(&Token::SemiColon, "`;`")
    }

    fn syntax(&self, expected: &'static str) -> Error {
        Error::Syntax {
            expected,
            found: self
                .peek()
                .map_or_else(|| "the end of the body".into(), ToString::to_string),
        }
    }

    /// Runs `read` one nesting level deeper, refusing past the limit.
    fn nested<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        if self.depth >= NESTING_LIMIT {
            return Err(Error::NestingTooDeep {
                limit: NESTING_LIMIT,
            });
        }
        self.depth += 1;
        let read = read(self);
        self.depth -= 1;
        read
    }

    /// Advances to the first token at bracket depth zero that `stop` accepts,
    /// returning the range passed over and leaving the stop token in place.
    ///
    /// Only brackets nest, as in PostgreSQL. Any other `;` is an error, which
    /// keeps a missing keyword from swallowing the statements after it.
    fn segment(
        &mut self,
        expected: &'static str,
        stop: impl Fn(&Token, Option<&Token>) -> bool,
    ) -> Result<Range<usize>, Error> {
        let start = self.index;
        let mut depth = 0_usize;
        loop {
            let Some(token) = self.peek() else {
                return Err(self.syntax(expected));
            };
            if depth == 0 && stop(token, self.peek_nth(1)) {
                return Ok(start..self.index);
            }
            match token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket if depth > 0 => depth -= 1,
                Token::RParen | Token::RBracket | Token::SemiColon => {
                    return Err(self.syntax(expected));
                }
                _ => {}
            }
            self.index += 1;
        }
    }

    /// Reads up to the first token `stop` accepts as the `SELECT` that
    /// evaluates an expression.
    fn expression(
        &mut self,
        expected: &'static str,
        stop: impl Fn(&Token, Option<&Token>) -> bool,
    ) -> Result<Box<Statement>, Error> {
        let range = self.segment(expected, stop)?;
        parse(&self.tokens[range], true)
    }

    /// Reads up to `keyword` as the `SELECT` that evaluates an expression.
    fn expression_until(&mut self, keyword: &'static str) -> Result<Box<Statement>, Error> {
        self.expression(keyword, |token, _| is_keyword(token, keyword))
    }

    /// Reads up to the `;` that ends the statement as one SQL statement.
    fn statement_until_semicolon(&mut self, select: bool) -> Result<Box<Statement>, Error> {
        let range = self.segment("`;`", |token, _| *token == Token::SemiColon)?;
        parse(&self.tokens[range], select)
    }

    /// Skips `#option` lines, which configure the PL/pgSQL compiler.
    fn compiler_options(&mut self) -> Result<(), Error> {
        while self.eat_token(&Token::Sharp) {
            for _ in 0..2 {
                if !matches!(self.peek(), Some(Token::Word(_))) {
                    return Err(self.syntax("a compiler option"));
                }
                self.index += 1;
            }
        }
        Ok(())
    }

    /// Skips a `<<label>>`, returning whether there was one.
    fn label(&mut self) -> Result<bool, Error> {
        if !self.eat_token(&Token::ShiftLeft) {
            return Ok(false);
        }
        self.name("a label")?;
        self.expect_token(&Token::ShiftRight, "`>>`")?;
        Ok(true)
    }

    /// Skips the label an `END` may repeat.
    fn end_label(&mut self) {
        if matches!(self.peek(), Some(Token::Word(_))) {
            self.index += 1;
        }
    }

    /// Reads an identifier, folded the way PostgreSQL folds an unquoted one.
    fn name(&mut self, expected: &'static str) -> Result<String, Error> {
        let Some(Token::Word(word)) = self.peek() else {
            return Err(self.syntax(expected));
        };
        let name = identifier(word);
        self.index += 1;
        Ok(name)
    }

    /// The query `name` binds, looked up from the innermost block outwards.
    fn bound_cursor(&self, name: &str) -> Option<&Statement> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev())
            .find(|declared| declared.name == name)
            .and_then(|declared| declared.cursor.as_deref())
    }

    fn block(&mut self) -> Result<Block, Error> {
        self.scopes.push(Vec::new());
        let mut declarations = Vec::new();
        if self.eat("DECLARE") {
            while !self.at("BEGIN") {
                // PostgreSQL accepts a repeated DECLARE inside the section.
                if !self.eat("DECLARE") {
                    self.declaration(&mut declarations)?;
                }
            }
        }
        self.expect("BEGIN")?;
        let statements = self.statements(&["EXCEPTION", "END"])?;
        let mut handlers = Vec::new();
        if self.eat("EXCEPTION") {
            loop {
                self.expect("WHEN")?;
                self.segment("THEN", |token, _| is_keyword(token, "THEN"))?;
                self.index += 1;
                handlers.push(self.statements(&["WHEN", "END"])?);
                if !self.at("WHEN") {
                    break;
                }
            }
        }
        self.expect("END")?;
        self.end_label();
        self.scopes.pop();
        Ok(Block {
            declarations,
            statements,
            handlers,
        })
    }

    fn declaration(&mut self, declarations: &mut Vec<Statement>) -> Result<(), Error> {
        let name = self.name("a declaration")?;
        let cursor_words = [
            &["CURSOR"][..],
            &["SCROLL", "CURSOR"],
            &["NO", "SCROLL", "CURSOR"],
        ]
        .into_iter()
        .find(|words| {
            words.iter().enumerate().all(|(offset, word)| {
                self.peek_nth(offset)
                    .is_some_and(|token| is_keyword(token, word))
            })
        });

        let cursor = if let Some(words) = cursor_words {
            self.index += words.len();
            if self.eat_token(&Token::LParen) {
                self.segment("`)`", |token, _| *token == Token::RParen)?;
                self.index += 1;
            }
            if !(self.eat("FOR") || self.eat("IS")) {
                return Err(self.syntax("FOR or IS"));
            }
            Some(self.statement_until_semicolon(false)?)
        } else {
            self.segment("`;`", |token, _| {
                matches!(token, Token::SemiColon | Token::Assignment | Token::Eq)
                    || is_keyword(token, "DEFAULT")
            })?;
            if self.peek() != Some(&Token::SemiColon) {
                self.index += 1;
                declarations.push(*self.statement_until_semicolon(true)?);
            }
            None
        };
        self.expect_semicolon()?;
        self.scopes
            .last_mut()
            .expect("a declaration sits inside a block")
            .push(Declared { name, cursor });
        Ok(())
    }

    /// Reads statements until one of `until` starts the next.
    fn statements(&mut self, until: &[&str]) -> Result<Vec<Node>, Error> {
        let mut nodes = Vec::new();
        while !self.at_any(until) {
            self.statement(&mut nodes)?;
        }
        Ok(nodes)
    }

    fn statement(&mut self, nodes: &mut Vec<Node>) -> Result<(), Error> {
        let labelled = self.label()?;
        let Some(Token::Word(word)) = self.peek() else {
            return Err(self.syntax("a statement"));
        };
        let command = Command::of(word);
        if labelled
            && !matches!(
                command,
                Some(
                    Command::Block
                        | Command::Loop
                        | Command::While
                        | Command::For
                        | Command::Foreach
                )
            )
        {
            return Err(self.syntax("a block or a loop after the label"));
        }

        match command {
            Some(Command::Block) => {
                let block = self.nested(Self::block)?;
                nodes.push(Node::Block(block));
            }
            Some(Command::If) => {
                let conditional = self.nested(Self::if_statement)?;
                nodes.push(Node::If(conditional));
            }
            Some(Command::Case) => {
                let case = self.nested(Self::case_statement)?;
                nodes.push(Node::Case(case));
            }
            Some(Command::Loop | Command::While | Command::For | Command::Foreach) => {
                let looped = self.nested(Self::loop_statement)?;
                nodes.push(Node::Loop(looped));
            }
            Some(Command::Exit) => self.exit(nodes)?,
            Some(Command::Return) => self.return_statement(nodes)?,
            Some(Command::Raise) => self.raise(nodes)?,
            Some(Command::Assert) => {
                self.index += 1;
                nodes.push(Node::Statement(self.expression("`;`", |token, _| {
                    matches!(token, Token::Comma | Token::SemiColon)
                })?));
                if self.eat_token(&Token::Comma) {
                    nodes.push(Node::Statement(self.statement_until_semicolon(true)?));
                }
            }
            Some(Command::Execute) => {
                self.index += 1;
                nodes.push(Node::Dynamic(self.dynamic(End::Semicolon)?));
            }
            Some(Command::Perform) => {
                self.index += 1;
                nodes.push(Node::Statement(self.statement_until_semicolon(true)?));
            }
            Some(Command::Open) => self.open(nodes)?,
            Some(Command::Fetch) => self.fetch(nodes)?,
            Some(Command::Silent) => {
                self.segment("`;`", |token, _| *token == Token::SemiColon)?;
            }
            None if self.at_assignment() => self.assignment(nodes)?,
            None => nodes.push(Node::Statement(self.sql_statement()?)),
        }
        self.expect_semicolon()
    }

    fn if_statement(&mut self) -> Result<If, Error> {
        self.index += 1;
        let mut branches = Vec::new();
        loop {
            let condition = self.expression_until("THEN")?;
            self.index += 1;
            let statements = self.statements(&["ELSIF", "ELSEIF", "ELSE", "END"])?;
            branches.push(Branch {
                condition,
                statements,
            });
            if !(self.eat("ELSIF") || self.eat("ELSEIF")) {
                break;
            }
        }
        let otherwise = self.otherwise()?;
        self.expect("IF")?;
        Ok(If {
            branches,
            otherwise,
        })
    }

    fn case_statement(&mut self) -> Result<Case, Error> {
        self.index += 1;
        let subject = if self.at("WHEN") {
            None
        } else {
            Some(self.expression_until("WHEN")?)
        };
        let mut branches = Vec::new();
        loop {
            self.expect("WHEN")?;
            let condition = self.expression_until("THEN")?;
            self.index += 1;
            let statements = self.statements(&["WHEN", "ELSE", "END"])?;
            branches.push(Branch {
                condition,
                statements,
            });
            if !self.at("WHEN") {
                break;
            }
        }
        let otherwise = self.otherwise()?;
        self.expect("CASE")?;
        Ok(Case {
            subject,
            branches,
            otherwise,
        })
    }

    /// Reads an optional `ELSE` and the `END` after it.
    fn otherwise(&mut self) -> Result<Vec<Node>, Error> {
        let otherwise = if self.eat("ELSE") {
            self.statements(&["END"])?
        } else {
            Vec::new()
        };
        self.expect("END")?;
        Ok(otherwise)
    }

    fn loop_statement(&mut self) -> Result<Loop, Error> {
        let control = if self.at("LOOP") {
            LoopControl::Unconditional
        } else if self.eat("WHILE") {
            LoopControl::While(self.expression_until("LOOP")?)
        } else if self.eat("FOR") {
            self.for_control()?
        } else {
            self.expect("FOREACH")?;
            self.segment("IN", |token, _| {
                is_keyword(token, "SLICE") || is_keyword(token, "IN")
            })?;
            if self.eat("SLICE") {
                self.segment("IN", |token, _| is_keyword(token, "IN"))?;
            }
            self.expect("IN")?;
            self.expect("ARRAY")?;
            LoopControl::Array(self.expression_until("LOOP")?)
        };
        self.expect("LOOP")?;
        let statements = self.statements(&["END"])?;
        self.expect("END")?;
        self.expect("LOOP")?;
        self.end_label();
        Ok(Loop {
            control,
            statements,
        })
    }

    fn for_control(&mut self) -> Result<LoopControl, Error> {
        self.segment("IN", |token, _| is_keyword(token, "IN"))?;
        self.index += 1;
        if self.eat("EXECUTE") {
            return Ok(LoopControl::Dynamic(self.dynamic(End::Loop)?));
        }
        let reverse = self.eat("REVERSE");
        if !reverse
            && let Some(Token::Word(word)) = self.peek()
            && (matches!(self.peek_nth(1), Some(Token::LParen))
                || self
                    .peek_nth(1)
                    .is_some_and(|token| is_keyword(token, "LOOP")))
            && let Some(query) = self.bound_cursor(&identifier(word)).cloned().map(Box::new)
        {
            self.index += 1;
            let arguments = if self.eat_token(&Token::LParen) {
                self.cursor_arguments()?
            } else {
                Vec::new()
            };
            return Ok(LoopControl::Cursor { arguments, query });
        }

        let first = self.segment("LOOP", |token, next| {
            is_keyword(token, "LOOP") || is_range_dots(token, next)
        })?;
        if self.at("LOOP") {
            if reverse {
                return Err(self.syntax("`..`"));
            }
            return Ok(LoopControl::Query(parse(&self.tokens[first], false)?));
        }
        let lower = parse(&self.tokens[first], true)?;
        self.index += 2;
        let upper = self.expression("LOOP", |token, _| {
            is_keyword(token, "BY") || is_keyword(token, "LOOP")
        })?;
        let step = if self.eat("BY") {
            Some(self.expression_until("LOOP")?)
        } else {
            None
        };
        Ok(LoopControl::Range { lower, upper, step })
    }

    /// Reads the arguments of a cursor after its `(`, through the `)`.
    ///
    /// A named argument's `name :=` or `name =>` is dropped, since only the
    /// value is evaluated.
    fn cursor_arguments(&mut self) -> Result<Vec<Statement>, Error> {
        let mut arguments = Vec::new();
        loop {
            if matches!(self.peek(), Some(Token::Word(_)))
                && matches!(self.peek_nth(1), Some(Token::Assignment | Token::RArrow))
            {
                self.index += 2;
            }
            arguments.push(*self.expression("`)`", |token, _| {
                matches!(token, Token::Comma | Token::RParen)
            })?);
            if !self.eat_token(&Token::Comma) {
                break;
            }
        }
        self.expect_token(&Token::RParen, "`)`")?;
        Ok(arguments)
    }

    /// Reads a command string and its `INTO` and `USING` clauses, in either
    /// order, up to `end`.
    fn dynamic(&mut self, end: End) -> Result<Dynamic, Error> {
        let clause_or_end = move |token: &Token| {
            end.at(token) || is_keyword(token, "INTO") || is_keyword(token, "USING")
        };
        let command = self.expression("the end of the command", |token, _| clause_or_end(token))?;
        let mut arguments = Vec::new();
        loop {
            if self.eat("INTO") {
                self.index = into_target_end(&self.tokens, self.index)
                    .ok_or_else(|| self.syntax("an INTO target"))?;
            } else if self.eat("USING") {
                loop {
                    arguments.push(*self.expression("the end of the command", |token, _| {
                        *token == Token::Comma || clause_or_end(token)
                    })?);
                    if !self.eat_token(&Token::Comma) {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        Ok(Dynamic { command, arguments })
    }

    fn exit(&mut self, nodes: &mut Vec<Node>) -> Result<(), Error> {
        self.index += 1;
        if matches!(self.peek(), Some(Token::Word(_))) && !self.at("WHEN") {
            self.index += 1;
        }
        if self.eat("WHEN") {
            nodes.push(Node::Statement(self.statement_until_semicolon(true)?));
        }
        Ok(())
    }

    fn return_statement(&mut self, nodes: &mut Vec<Node>) -> Result<(), Error> {
        self.index += 1;
        if self.eat("QUERY") {
            if self.eat("EXECUTE") {
                nodes.push(Node::Dynamic(self.dynamic(End::Semicolon)?));
            } else {
                nodes.push(Node::Statement(self.statement_until_semicolon(false)?));
            }
            return Ok(());
        }
        self.eat("NEXT");
        if self.peek() != Some(&Token::SemiColon) {
            nodes.push(Node::Statement(self.statement_until_semicolon(true)?));
        }
        Ok(())
    }

    fn raise(&mut self, nodes: &mut Vec<Node>) -> Result<(), Error> {
        const LEVELS: [&str; 6] = ["DEBUG", "LOG", "INFO", "NOTICE", "WARNING", "EXCEPTION"];
        self.index += 1;
        if self.peek() == Some(&Token::SemiColon) {
            return Ok(());
        }
        if self.at_any(&LEVELS) {
            self.index += 1;
        }
        if self.peek().is_some_and(is_string_literal) {
            self.index += 1;
            while self.eat_token(&Token::Comma) {
                nodes.push(Node::Statement(self.expression("`;`", |token, _| {
                    matches!(token, Token::Comma | Token::SemiColon) || is_keyword(token, "USING")
                })?));
            }
        } else if self.eat("SQLSTATE") {
            if !self.peek().is_some_and(is_string_literal) {
                return Err(self.syntax("an SQLSTATE code"));
            }
            self.index += 1;
        } else if matches!(self.peek(), Some(Token::Word(_))) && !self.at("USING") {
            self.index += 1;
        }
        if self.eat("USING") {
            loop {
                self.name("a RAISE option")?;
                if !(self.eat_token(&Token::Eq) || self.eat_token(&Token::Assignment)) {
                    return Err(self.syntax("`=`"));
                }
                nodes.push(Node::Statement(self.expression("`;`", |token, _| {
                    matches!(token, Token::Comma | Token::SemiColon)
                })?));
                if !self.eat_token(&Token::Comma) {
                    break;
                }
            }
        }
        Ok(())
    }

    fn open(&mut self, nodes: &mut Vec<Node>) -> Result<(), Error> {
        self.index += 1;
        let name = self.name("a cursor name")?;
        if self.eat("NO") {
            self.expect("SCROLL")?;
        } else {
            self.eat("SCROLL");
        }
        if self.eat("FOR") {
            if self.eat("EXECUTE") {
                nodes.push(Node::Dynamic(self.dynamic(End::Semicolon)?));
            } else {
                nodes.push(Node::Statement(self.statement_until_semicolon(false)?));
            }
            return Ok(());
        }
        let Some(query) = self.bound_cursor(&name).cloned().map(Box::new) else {
            return Err(Error::UnboundCursor { name });
        };
        if self.eat_token(&Token::LParen) {
            nodes.extend(
                self.cursor_arguments()?
                    .into_iter()
                    .map(|argument| Node::Statement(Box::new(argument))),
            );
        }
        nodes.push(Node::Statement(query));
        Ok(())
    }

    /// Reads `FETCH` or `MOVE`, whose direction may count rows with an
    /// expression.
    fn fetch(&mut self, nodes: &mut Vec<Node>) -> Result<(), Error> {
        const DIRECTIONS: [&str; 9] = [
            "NEXT", "PRIOR", "FIRST", "LAST", "ABSOLUTE", "RELATIVE", "FORWARD", "BACKWARD", "ALL",
        ];
        self.index += 1;
        let direction = self.segment("`;`", |token, _| {
            *token == Token::SemiColon || is_keyword(token, "FROM") || is_keyword(token, "IN")
        })?;
        if self.peek() != Some(&Token::SemiColon) {
            let count = &self.tokens[direction];
            let skipped = count
                .iter()
                .take_while(|token| DIRECTIONS.iter().any(|word| is_keyword(token, word)))
                .count();
            if skipped < count.len() {
                nodes.push(Node::Statement(parse(&count[skipped..], true)?));
            }
            self.segment("`;`", |token, _| *token == Token::SemiColon)?;
        }
        Ok(())
    }

    /// True when the statement starts with an assignment target, a name
    /// with any fields and subscripts, followed by `:=` or `=`.
    fn at_assignment(&self) -> bool {
        let mut at = self.index + 1;
        loop {
            match self.tokens.get(at) {
                Some(Token::Period) => at += 2,
                Some(Token::LBracket) => {
                    let mut depth = 0_usize;
                    loop {
                        match self.tokens.get(at) {
                            Some(Token::LBracket) => depth += 1,
                            Some(Token::RBracket) => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            None | Some(Token::SemiColon) => return false,
                            _ => {}
                        }
                        at += 1;
                    }
                    at += 1;
                }
                Some(Token::Assignment | Token::Eq) => return true,
                _ => return false,
            }
        }
    }

    /// Reads an assignment, yielding each subscript of its target and then
    /// its value.
    fn assignment(&mut self, nodes: &mut Vec<Node>) -> Result<(), Error> {
        self.index += 1;
        loop {
            match self.peek() {
                Some(Token::Period) => self.index += 2,
                Some(Token::LBracket) => {
                    self.index += 1;
                    loop {
                        nodes.push(Node::Statement(self.expression("`]`", |token, _| {
                            matches!(token, Token::Colon | Token::RBracket)
                        })?));
                        if !self.eat_token(&Token::Colon) {
                            break;
                        }
                    }
                    self.index += 1;
                }
                _ => break,
            }
        }
        self.index += 1;
        nodes.push(Node::Statement(self.statement_until_semicolon(true)?));
        Ok(())
    }

    /// Reads an SQL statement up to its `;`, dropping its `INTO` target the
    /// way PostgreSQL does.
    ///
    /// An `INTO` right after `INSERT` or `MERGE`, or anywhere in `IMPORT`,
    /// belongs to the SQL. A routine defined with `BEGIN ATOMIC` keeps the
    /// `;` inside its body, which PostgreSQL finds by counting `BEGIN` and
    /// `CASE` against `END`.
    fn sql_statement(&mut self) -> Result<Box<Statement>, Error> {
        let start = self.index;
        let starts = |offset: usize, keyword: &str| {
            self.tokens
                .get(start + offset)
                .is_some_and(|token| is_keyword(token, keyword))
        };
        let defines_routine = starts(0, "CREATE") && {
            let name = if starts(1, "OR") && starts(2, "REPLACE") {
                3
            } else {
                1
            };
            starts(name, "FUNCTION") || starts(name, "PROCEDURE")
        };
        let imports = starts(0, "IMPORT");
        let mut parens = 0_usize;
        let mut begins = 0_usize;
        let mut into: Option<Range<usize>> = None;

        loop {
            let Some(token) = self.peek() else {
                return Err(self.syntax("`;`"));
            };
            match token {
                Token::SemiColon if parens == 0 && begins == 0 => break,
                Token::LParen => parens += 1,
                Token::RParen => parens = parens.saturating_sub(1),
                _ => {}
            }
            if defines_routine && parens == 0 {
                if is_keyword(token, "BEGIN") || is_keyword(token, "CASE") {
                    begins += 1;
                } else if is_keyword(token, "END") && begins > 0 {
                    begins -= 1;
                }
            }
            let into_target = is_keyword(token, "INTO")
                && !imports
                && !self.index.checked_sub(1).is_some_and(|before| {
                    is_keyword(&self.tokens[before], "INSERT")
                        || is_keyword(&self.tokens[before], "MERGE")
                });
            if into_target {
                if into.is_some() {
                    return Err(self.syntax("a single INTO"));
                }
                let end = into_target_end(&self.tokens, self.index + 1)
                    .ok_or_else(|| self.syntax("an INTO target"))?;
                into = Some(self.index..end);
                self.index = end;
            } else {
                self.index += 1;
            }
        }

        match into {
            None => parse(&self.tokens[start..self.index], false),
            Some(into) => {
                let mut kept = Vec::with_capacity(self.index - start - into.len());
                kept.extend_from_slice(&self.tokens[start..into.start]);
                kept.extend_from_slice(&self.tokens[into.end..self.index]);
                parse(&kept, false)
            }
        }
    }
}

/// Where the `INTO` target that starts at `at` ends: an optional `STRICT`,
/// then names with any fields, separated by commas.
///
/// `None` when no name follows.
fn into_target_end(tokens: &[Token], mut at: usize) -> Option<usize> {
    if tokens
        .get(at)
        .is_some_and(|token| is_keyword(token, "STRICT"))
    {
        at += 1;
    }
    if !matches!(tokens.get(at), Some(Token::Word(_))) {
        return None;
    }
    loop {
        at += 1;
        while matches!(tokens.get(at), Some(Token::Period))
            && matches!(tokens.get(at + 1), Some(Token::Word(_)))
        {
            at += 2;
        }
        if !(matches!(tokens.get(at), Some(Token::Comma))
            && matches!(tokens.get(at + 1), Some(Token::Word(_))))
        {
            return Some(at);
        }
        at += 1;
    }
}

/// Parses `tokens` as one SQL statement, behind a `SELECT` when `select`
/// holds.
fn parse(tokens: &[Token], select: bool) -> Result<Box<Statement>, Error> {
    let mut owned = Vec::with_capacity(tokens.len() + 1);
    if select {
        owned.push(Token::Word(Word {
            value: "SELECT".into(),
            quote_style: None,
            keyword: Keyword::SELECT,
        }));
    }
    owned.extend_from_slice(tokens);
    let mut parser = Parser::new(&PostgreSqlDialect {}).with_tokens(owned);
    parser
        .parse_statement()
        .and_then(|statement| {
            parser
                .expect_token(&Token::EOF)
                .map(|_| Box::new(statement))
        })
        .map_err(|source| Error::EmbeddedSql {
            sql: render(tokens, select),
            source,
        })
}

/// The text of `tokens`, one space apart, for an error message.
fn render(tokens: &[Token], select: bool) -> String {
    let mut text = String::from(if select { "SELECT" } else { "" });
    for token in tokens {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&token.to_string());
    }
    text
}

/// True for `keyword` written unquoted, in any case.
fn is_keyword(token: &Token, keyword: &str) -> bool {
    matches!(token, Token::Word(word) if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(keyword))
}

/// True for the `..` of an integer range, which [`spaced_ranges`] split into
/// two periods.
fn is_range_dots(token: &Token, next: Option<&Token>) -> bool {
    *token == Token::Period && next == Some(&Token::Period)
}

fn is_string_literal(token: &Token) -> bool {
    matches!(
        token,
        Token::SingleQuotedString(_)
            | Token::EscapedStringLiteral(_)
            | Token::DollarQuotedString(_)
            | Token::UnicodeStringLiteral(_)
            | Token::NationalStringLiteral(_)
    )
}

/// The name an identifier denotes: as written when quoted, ASCII-folded to
/// lower case otherwise, as PostgreSQL folds it.
fn identifier(word: &Word) -> String {
    if word.quote_style.is_some() {
        word.value.clone()
    } else {
        word.value.to_ascii_lowercase()
    }
}
