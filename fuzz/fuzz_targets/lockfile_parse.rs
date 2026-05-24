#![no_main]

use agentmesh_core::lockfile::Lockfile;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let _ = serde_yml::from_str::<Lockfile>(input);
    }
});
