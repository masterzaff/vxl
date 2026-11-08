//! A region file (`r.X.Z.mca`) as found on disk: parse it, check its layout, and
//! decompress every chunk it yields. Damage must be an error, never a panic.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vxl::anvil;

fuzz_target!(|data: &[u8]| {
    let _ = anvil::check_layout(data);
    if let Ok(p) = anvil::parse_region(data, Some((0, 0)), |_| Err(std::io::ErrorKind::NotFound.into())) {
        for c in p.chunks {
            let _ = anvil::decompress(c.compression, &c.payload);
        }
    }
});
