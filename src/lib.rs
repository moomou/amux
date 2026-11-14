//! amux: a crash-proof, agent-aware terminal multiplexer (tmux compatible).

pub mod agents;
pub mod bridge;
pub mod cell;
pub mod cli;
pub mod cmd;
pub mod copymode;
pub mod format;
pub mod history;
pub mod keeper;
pub mod keys;
pub mod migrate;
pub mod model;
pub mod options;
pub mod paths;
pub mod proto;
pub mod pty;
pub mod server;
pub mod term;
pub mod tmux;
pub mod util;
