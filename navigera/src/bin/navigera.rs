//! `navigera` — thin CLI wrapper over the [`navigera`] library.
//!
//! All parsing, protocol and session logic lives in `navigera::
//! navigera` so it can be reused (and unit-tested) without spawning a
//! process; this binary only wires argv/stdin/stdout to that library.
//!
//! ```text
//! navigera eval --expression "() => document.title" --pretty   # one-shot
//! navigera serve                                               # warm session
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    navigera::cli::main(std::env::args())
}
