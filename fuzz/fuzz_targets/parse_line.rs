#![no_main]

use bones_core::event::parser::parse_line;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 * 1024 {
        return;
    }
    if let Ok(line) = std::str::from_utf8(data) {
        let _ = parse_line(line);
    }
});
