#![no_main]

use basalt::sql::parser::parse;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    let _ = parse(input);
});
