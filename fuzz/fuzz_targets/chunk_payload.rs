//! One chunk payload: first byte is the compression type (gzip, zlib, none, LZ4, or
//! anything else), the rest is the payload. Includes Minecraft's LZ4 block framing.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vxl::anvil;

fuzz_target!(|data: &[u8]| {
    if let Some((&comp, payload)) = data.split_first() {
        let _ = anvil::decompress(comp % 8, payload);
    }
});
