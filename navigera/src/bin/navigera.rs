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

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use navigera::protocol::{self, ArgsError};

fn main() -> ExitCode {
    // Panics now go to stderr, which stays clean: Chrome's own stderr is piped
    // (and drained) inside the launch, so its DBus noise can't mask our output.
    let parsed = match navigera::protocol::parse_args(std::env::args()) {
        Ok(parsed) => parsed,
        Err(ArgsError::Help(text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(ArgsError::Invalid(message)) => {
            eprintln!("navigera: {message}");
            return ExitCode::from(2);
        }
    };

    let stdout = io::stdout();
    let mut output = stdout.lock();
    let code = run(&parsed, &mut output);
    let _ = output.flush();
    code
}

/// Route to the right mode: one-shot, stdin `serve`, or a named session
/// (server, detached `start`, or a client call against a warm browser).
fn run(parsed: &protocol::ParsedArgs, output: &mut dyn Write) -> ExitCode {
    let config = &parsed.config;
    if let Some(local) = &parsed.local {
        return protocol::run_local(local, config, output);
    }
    if parsed.mcp {
        let stdin = io::stdin();
        return navigera::mcp::serve(config, &mut stdin.lock(), output);
    }
    {
        use navigera::session;
        // An explicit `--session` always wins; $NAVIGERA_SESSION only
        // routes client calls and `start` (a bare `serve` stays on stdin).
        if parsed.start {
            return match config.session.clone().or_else(session::env_session) {
                Some(name) => session::start(config, &name, output),
                None => {
                    eprintln!("navigera: `start` needs --session <name>");
                    ExitCode::from(2)
                }
            };
        }
        match (&parsed.command, &config.session) {
            (None, Some(name)) => return session::serve_socket(config, name),
            (Some(command), Some(name)) => return session::client(config, name, command, output),
            (Some(command), None) => {
                if let Some(name) = session::env_session() {
                    return session::client(config, &name, command, output);
                }
            }
            (None, None) => {}
        }
    }
    match &parsed.command {
        Some(command) => protocol::oneshot(config, command, output),
        None => {
            let stdin = io::stdin();
            let mut input: Box<dyn BufRead> = Box::new(stdin.lock());
            protocol::serve(config, input.as_mut(), output)
        }
    }
}
