//! Phase timings on stderr, when `COMPARA_PHASES` is set.
//!
//! A comparison has costs that move independently: sweeping and hashing every
//! row, inserting them into the index, joining, and rendering. Knowing which one
//! grew is the difference between tuning and guessing, and the cores-busy column
//! says only *that* something is serial, never which thing.
//!
//! The same switch and the same output shape every other port uses. It lives
//! here rather than inside one engine because both of this port's paths want
//! it: the text engine had it from the start, and the columnar Parquet reader
//! was the one path in any of the four ports that could not be asked where its
//! time went.

use std::time::Instant;

pub struct Phases {
    on: bool,
    tag: &'static str,
    last: Instant,
}

impl Phases {
    /// `tag` prefixes every line, because the two files are read on two threads
    /// and their phases would otherwise interleave unattributed.
    pub fn new(tag: &'static str) -> Self {
        Phases {
            on: std::env::var_os("COMPARA_PHASES").is_some(),
            tag,
            last: Instant::now(),
        }
    }

    pub fn mark(&mut self, what: &str) {
        let now = Instant::now();
        if self.on {
            let name = format!("{}{}", self.tag, what);
            eprintln!("  {:<26} {:7.3}s", name, (now - self.last).as_secs_f64());
        }
        self.last = now;
    }
}
