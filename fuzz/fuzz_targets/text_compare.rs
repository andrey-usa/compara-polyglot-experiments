#![no_main]
// CSV / TSV / NDJSON readers (format is sniffed from content). Input = A, 0xFF separator, B.
#[path = "hook.rs"] mod hook;
use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;
fn dir() -> PathBuf { let d = PathBuf::from(format!("/tmp/fz-text-{}", std::process::id())); let _ = std::fs::create_dir_all(&d); d }
fuzz_target!(|data: &[u8]| {
    hook::arm("text_compare", data);
    let d = dir();
    let (a, b) = match data.iter().position(|&c| c == 0xFF) { Some(i) => (&data[..i], &data[i + 1..]), None => (data, data) };
    let (pa, pb) = (d.join("a"), d.join("b"));
    std::fs::write(&pa, a).unwrap(); std::fs::write(&pb, b).unwrap();
    let mut o = compara::Options::default();
    o.threads = Some(1);
    let shape = match compara::engine::turbo::shape(&pa, &o) { Ok(s) => s, Err(_) => return };
    if shape.columns.is_empty() { return; }
    o.key = vec![shape.columns[0].clone()];
    let _ = compara::compare(&pa, &pb, &mut o);
});
