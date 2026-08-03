//! comet-proto — wire types shared by engine, UI, and RPC.
//!
//! Ported from comet's `packages/control/src/wire.ts` + `packages/harness/src/types.ts`.
//! Token-usage *display* types are excluded by design; the `Usage` agent event is kept as a
//! harness-level passthrough (rate-limit meters), never persisted into docs.

pub mod agent;
pub mod claude_history;
pub mod codex_history;
pub mod entities;
pub mod motion;
pub mod view;

pub use agent::*;
pub use claude_history::*;
pub use codex_history::*;
pub use entities::*;
