//! amux: a crash-proof, agent-aware terminal multiplexer (tmux compatible).

pub mod agents;
pub mod cell;
pub mod cmd;
pub mod copymode;
pub mod format;
pub mod history;
pub mod keeper;
pub mod keys;
pub mod model;
pub mod options;
pub mod paths;
pub mod proto;
pub mod pty;
pub mod term;
pub mod util;
