#![no_main]
// Parquet footer + pages. Input = file A bytes, 0xFF-free split not used: B is the same bytes.
#[path = "hook.rs"] mod hook;
use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;
fuzz_target!(|data: &[u8]| {
    hook::arm("parquet_compare", data);
    let d = PathBuf::from(format!("/tmp/fz-pq-{}", std::process::id())); let _ = std::fs::create_dir_all(&d);
    let pa = d.join("a.parquet");
    std::fs::write(&pa, data).unwrap();
    let mut o = compara::Options::default();
    o.threads = Some(1);
    let shape = match compara::engine::turbo::shape(&pa, &o) { Ok(s) => s, Err(_) => return };
    if shape.columns.is_empty() { return; }
    o.key = vec![shape.columns[0].clone()];
    let _ = compara::compare(&pa, &pa, &mut o);
});
