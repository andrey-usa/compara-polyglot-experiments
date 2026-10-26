//! From-scratch minimal CDP engine: no external browser-automation crate.
//!
//! Design goals (this is what makes it faster than the alternatives):
//! * `Runtime.evaluate` with `returnByValue: true` — object results come back
//!   in ONE round trip (no evaluate + callFunctionOn + releaseObject dance).
//! * A tiny tokio runtime (2 workers) owned by the engine — no framework
//!   machinery, no background tasks beyond the single socket pump.
//! * Boring, direct teardown: SIGKILL the browser's process group, delete
//!   the throwaway profile (on tmpfs where possible, see `profile`), reap
//!   in the background. No close lifecycles, no nested runtimes.
//! * Synchronous public facade: the CLI stays simple, `block_on` per call.
//!
//! Layout: `transport` launches Chrome and finds the DevTools endpoint,
//! `client` multiplexes JSON-RPC over the websocket, `browser` owns the
//! browser-level connection, `page` implements the tab-level operations.

pub mod attach;
pub mod ax;
mod browser;
mod client;
pub mod downloads;
mod events;
pub mod import;
pub mod logs;
mod page;
mod procjob;
mod profile;
pub(crate) mod transport;

pub use browser::{Browser, LaunchOptions};
pub use events::{glob_match, DialogPolicy, Route, RouteAction};
pub use page::{locate_fn as locate_js, AxOptions, ClickOptions, Locator, Page, Target, DEEP, DEFAULT_ELEMENT_WAIT};

/// Element functions shared with the session layer.
pub(crate) mod page_js {
    pub(crate) use super::page::{COMBO_FOCUS, COMBO_PICK, FOCUS_EL, FOCUS_SELECT_ALL, GET_EL, IS_EL, MOUSE_AT};
}
