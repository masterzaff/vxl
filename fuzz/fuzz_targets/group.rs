//! Stored group objects (repository format 3). Damage must be an error, never a panic or
//! a runaway allocation, whatever the sources a reference points at return.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vxl::group;

fuzz_target!(|data: &[u8]| {
    let _ = group::sources(data);
    if let Ok(body) = group::Body::decode(data) {
        let _ = body.chunk(|i, k| Ok(body.slot(k ^ i as u32).unwrap_or_default().to_vec()));
    }
});
