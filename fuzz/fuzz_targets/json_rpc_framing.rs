#![no_main]

use std::io::{BufReader, Cursor};

use agentmesh_protocol::{JsonRpcRequest, read_frame, read_json_frame, write_frame};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut reader = BufReader::new(Cursor::new(data));
    if let Ok(payload) = read_frame(&mut reader) {
        let mut encoded = Vec::new();
        let _ = write_frame(&mut encoded, &payload);
    }

    let mut reader = BufReader::new(Cursor::new(data));
    let _ = read_json_frame::<JsonRpcRequest>(&mut reader);
});
