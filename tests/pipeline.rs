//! The stages composed over real function bodies, through the public API only.
//!
//! The unit tests cover each stage alone. These run whole fixtures through the
//! crate the way a dependent sees it, so a break in the handoff between stages
//! fails here even when every stage is individually correct.

use std::{fs, path::PathBuf};

use sqlparser_plpgsql::{PlPgSqlPreprocessor, Scanner};

/// Every fixture body, as file stem and contents.
fn fixtures() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/fixtures");
    let mut bodies: Vec<(String, String)> = fs::read_dir(&dir)
        .unwrap_or_else(|error| {
            panic!("fixture directory {} is unreadable: {error}", dir.display())
        })
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "sql"))
        .map(|path| {
            let name = path
                .file_stem()
                .expect("a file name")
                .to_string_lossy()
                .into_owned();
            let body = fs::read_to_string(&path).expect("a readable fixture");
            (name, body)
        })
        .collect();
    bodies.sort();
    assert!(!bodies.is_empty(), "no fixtures found in {}", dir.display());
    bodies
}

#[test]
fn the_scanner_maps_every_byte_of_every_fixture() {
    for (name, body) in fixtures() {
        let regions = Scanner::new(&body).regions();
        assert_eq!(
            regions.len(),
            body.len(),
            "{name}: the region map must cover every byte"
        );
    }
}

#[test]
fn no_fixture_leaves_a_dollar_quote_open() {
    for (name, body) in fixtures() {
        assert!(
            !Scanner::new(&body).has_unterminated_dollar_quote(),
            "{name}: a fixture with an open dollar quote would silently truncate every later stage"
        );
    }
}

#[test]
fn every_fixture_preprocesses_into_non_empty_sql() {
    for (name, body) in fixtures() {
        let (sql, _context) = PlPgSqlPreprocessor::preprocess(&body)
            .unwrap_or_else(|error| panic!("{name}: a real body must preprocess: {error}"));
        assert!(
            !sql.trim().is_empty(),
            "{name}: preprocessing must not erase the body"
        );
    }
}

/// The crate's own scanner and the tokenizer inside `parse_body` are separate
/// mechanisms, so a fixture where they disagree about the block is a real bug.
///
/// Only block presence is compared. A body can be found and still fail to
/// parse, which `large_concatenated` does by design: it is several bodies in
/// one file, so the first `BEGIN` and the last `END` span all of them.
#[cfg(feature = "body-parse")]
#[test]
fn the_scanner_and_the_parser_agree_on_whether_a_block_is_present() {
    use sqlparser_plpgsql::{Error, parse_body};

    for (name, body) in fixtures() {
        let scanner_sees_begin = Scanner::new(body.trim()).find_keyword("BEGIN", 0).is_some();
        let parser_found_block = !matches!(
            parse_body(&name, &body),
            Err(Error::MissingBeginBlock { .. })
        );
        assert_eq!(
            scanner_sees_begin, parser_found_block,
            "{name}: the scanner and the tokenizer disagree about whether a block is present"
        );
    }
}

/// Every fixture holding one real body is readable. The adversarial inputs
/// hold none, and `large_concatenated` holds several in one file.
#[cfg(feature = "body-parse")]
#[test]
fn every_single_body_fixture_reads_to_its_embedded_sql() {
    const NOT_ONE_BODY: [&str; 3] = [
        "adversarial_nested",
        "adversarial_unicode",
        "large_concatenated",
    ];

    for (name, body) in fixtures() {
        if NOT_ONE_BODY.contains(&name.as_str()) {
            continue;
        }
        if let Err(error) = sqlparser_plpgsql::embedded_sql(&body) {
            panic!("{name}: a real body must be readable: {error}");
        }
    }
}
