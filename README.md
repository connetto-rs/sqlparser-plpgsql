# sqlparser-plpgsql

[![CI](https://github.com/connetto-rs/sqlparser-plpgsql/actions/workflows/ci.yml/badge.svg)](https://github.com/connetto-rs/sqlparser-plpgsql/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/connetto-rs/sqlparser-plpgsql/graph/badge.svg)](https://codecov.io/gh/connetto-rs/sqlparser-plpgsql)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/connetto-rs/sqlparser-plpgsql/blob/main/LICENSE)
[![crates.io](https://img.shields.io/crates/v/sqlparser-plpgsql.svg)](https://crates.io/crates/sqlparser-plpgsql)
[![docs.rs](https://docs.rs/sqlparser-plpgsql/badge.svg)](https://docs.rs/sqlparser-plpgsql)

Reading a PL/pgSQL function body means knowing which bytes are live code and which sit inside a string, a comment, or a dollar-quoted literal. Every transform here shares one scanner that answers exactly that, so a keyword inside a literal is never mistaken for a keyword. The crate is `no_std`, needing only `alloc`.

`Scanner` classifies each byte and searches only the live parts.

```rust
use sqlparser_plpgsql::Scanner;

let body = "IF x THEN y := 'THEN inside a string'; END IF";
let scanner = Scanner::new(body);

assert_eq!(scanner.find_keyword("THEN", 0), Some(5));
assert_eq!(scanner.find_keyword("THEN", 6), None);
```

Dollar-quoted literals have no SQLite spelling, so they are re-emitted as ordinary single-quoted ones with any inner quote doubled.

```rust
use sqlparser_plpgsql::Scanner;

let scanner = Scanner::new("msg := $tag$it's here$tag$");
assert_eq!(scanner.requote_dollar_literals(), "msg := 'it''s here'");
```

`PlPgSqlPreprocessor` rewrites a whole body into shapes a SQL parser accepts, turning `:=` assignments into `SET`, lifting `DECLARE` defaults into a context, and folding `SELECT INTO` into scalar subqueries. It returns that context alongside the rewritten text.

```rust
use sqlparser_plpgsql::PlPgSqlPreprocessor;

let (sql, _context) = PlPgSqlPreprocessor::preprocess("BEGIN\n  total := 1;\nEND")
    .expect("a body with no qualified assignment");
assert_eq!(sql, "BEGIN\n  SET total = 1;\nEND");
```

Enabling the `body-parse` feature adds `parse_body`, which preprocesses, tokenizes, and parses a body into a `sqlparser` `BeginEndStatements`, refusing the shapes that cannot survive the trip: an exception handler, an unterminated dollar quote, or a missing `BEGIN` or `END`.

The same feature adds `embedded_sql`, which reads every SQL statement and expression a body runs, in a tree of its blocks, branches, loops and exception handlers. Each expression appears as the `SELECT` PostgreSQL evaluates it with, a cursor's query appears where the cursor opens, and `EXECUTE` is reported as SQL built at run time.

Licensed under the MIT license.
