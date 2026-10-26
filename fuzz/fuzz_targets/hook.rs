use std::sync::{Mutex, Once};
pub static LAST: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static INIT: Once = Once::new();
pub fn arm(name: &'static str, data: &[u8]) {
    INIT.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(d) = LAST.lock() {
                let mut h: u64 = 1469598103934665603;
                for b in d.iter() { h = (h ^ *b as u64).wrapping_mul(1099511628211); }
                let _ = std::fs::write(format!("artifacts/{name}-panic-{h:016x}"), &*d);
            }
            prev(info);
        }));
    });
    let mut l = LAST.lock().unwrap(); l.clear(); l.extend_from_slice(data);
}
