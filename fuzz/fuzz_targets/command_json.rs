#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) {
        let items = match v { serde_json::Value::Array(a) => a, o => vec![o] };
        for mut it in items {
            if let serde_json::Value::Object(m) = &mut it { m.remove("id"); }
            if let Ok(c) = serde_json::from_value::<navigera::protocol::Command>(it) {
                let j = serde_json::to_string(&c).unwrap();
                let back: navigera::protocol::Command = serde_json::from_str(&j).expect("roundtrip");
                assert_eq!(c, back);
            }
        }
    }
});
