#![no_main]

use bones_core::db::migrations;
use bones_core::db::project::Projector;
use bones_core::event::parser::{ParsedLine, parse_line};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 * 1024 {
        return;
    }

    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    let Ok(mut conn) = rusqlite::Connection::open_in_memory() else {
        return;
    };
    if migrations::migrate(&mut conn).is_err() {
        return;
    }

    let projector = Projector::new(&conn);
    for line in input.lines() {
        let Ok(parsed) = parse_line(line) else {
            continue;
        };
        let ParsedLine::Event(event) = parsed else {
            continue;
        };
        let _ = projector.project_event(&event);
    }
});
