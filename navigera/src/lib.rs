//! navigera: from-scratch CDP CLI for AI-agentic browser exploration.
//!
//! Native Rust Chrome DevTools Protocol engine (no WebDriver, no external
//! driver crates): direct CDP JSON-RPC over WebSocket, two-worker Tokio
//! runtime, JSON-lines protocol with `serve` mode and one-shot subcommands.

pub mod browser;
pub mod cdp;
pub mod decide;
pub mod emulate;
pub mod mcp;
pub mod proc;
pub mod protocol;
pub mod session;
pub mod timing;
