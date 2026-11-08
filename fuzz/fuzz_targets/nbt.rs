//! Chunk NBT, in both parser modes. If it parses, writing it back must give the
//! exact same bytes (the parser is byte-exact by design).
#![no_main]
use libfuzzer_sys::fuzz_target;
use vxl::nbt;

fuzz_target!(|data: &[u8]| {
    for ext in [false, true] {
        if let Ok(root) = nbt::parse(data, ext) {
            assert_eq!(root.write(), data, "ext={ext}");
        }
    }
});
