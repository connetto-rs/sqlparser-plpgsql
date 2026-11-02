#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if text.len() > 2048 {
            return;
        }
        let _ = sqlparser_plpgsql::embedded_sql(text);
    }
});
