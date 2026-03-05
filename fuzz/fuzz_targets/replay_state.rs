#![no_main]

use std::collections::HashMap;

use bones_core::crdt::item_state::WorkItemState;
use bones_core::event::parser::{ParsedLine, parse_line};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 * 1024 {
        return;
    }

    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    let mut states: HashMap<String, WorkItemState> = HashMap::new();
    for line in input.lines() {
        let Ok(parsed) = parse_line(line) else {
            continue;
        };
        let ParsedLine::Event(event) = parsed else {
            continue;
        };
        let item_id = event.item_id.to_string();
        states.entry(item_id).or_default().apply_event(&event);
    }
});
