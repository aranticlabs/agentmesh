#![no_main]

use agentmesh_adapter_sdk_rust::{canonicalize_frontmatter, parse_frontmatter};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(markdown) = std::str::from_utf8(data) {
        let _ = parse_frontmatter(markdown);
        let _ = canonicalize_frontmatter(markdown);
    }
});
