//! Generate a synthetic world to play with:
//!
//!     cargo run --release --example gen_world -- /tmp/world [regions-per-side] [edit-round]
//!
//! Re-running with a higher edit-round "plays" in ~3% of chunks.

use vxl::anvil::Compression;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: gen_world <dir> [regions-per-side] [edit-round]");
    let side: i32 = args.next().map(|s| s.parse().unwrap()).unwrap_or(2);
    let round: u32 = args.next().map(|s| s.parse().unwrap()).unwrap_or(0);
    let regions: Vec<(i32, i32)> =
        (0..side).flat_map(|x| (0..side).map(move |z| (x - side / 2, z - side / 2))).collect();
    let edits = |cx: i32, cz: i32| {
        let touched = round > 0 && (cx * 31 + cz * 17 + round as i32).rem_euclid(32) == 0;
        if touched { round } else { 0 }
    };
    vxl::synth::write_world(dir.as_ref(), 42, &regions, 1024, Compression::Zlib, edits)?;
    println!("wrote {} regions to {dir}", regions.len());
    Ok(())
}
