#![no_main]
// Delta log (_delta_log/*.json commits). Input split on 0xFF into commit files 0,1,2...
#[path = "hook.rs"] mod hook;
use libfuzzer_sys::fuzz_target;
use compara::source::{Source, Want};
use std::path::PathBuf;
fuzz_target!(|data: &[u8]| {
    hook::arm("delta_log", data);
    let t = PathBuf::from(format!("/tmp/fz-delta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    let log = t.join("tbl/_delta_log"); std::fs::create_dir_all(&log).unwrap();
    for (i, part) in data.split(|&c| c == 0xFF).take(4).enumerate() {
        std::fs::write(log.join(format!("{:020}.json", i)), part).unwrap();
    }
    let spec = t.join("tbl").to_string_lossy().into_owned();
    let _ = Source::open(&spec, &Want::all());
    let _ = Source::open(&format!("{spec}@v1"), &Want::all());
});
