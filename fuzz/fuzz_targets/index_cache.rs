//! `cache/packs.idx`: any damage must decode to `None` (ignored and rewritten), never a
//! panic or a huge allocation.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vxl::index_cache;

fuzz_target!(|data: &[u8]| {
    let _ = index_cache::decode(data);
});
