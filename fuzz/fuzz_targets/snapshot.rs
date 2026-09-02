#![no_main]

use basalt::storage::read_snapshot_bytes;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let _ = read_snapshot_bytes(input);
});
