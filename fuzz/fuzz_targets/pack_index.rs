//! A pack file's trailer and index, as read when a repo is opened.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vxl::pack;

fuzz_target!(|data: &[u8]| {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, data).unwrap();
    let _ = pack::read_index(f.path());
});
