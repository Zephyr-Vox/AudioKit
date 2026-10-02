//! Synchronous production graphs. Hosts supply codecs/processing and own scheduling/transport.
pub mod capture;
pub mod receive;
pub mod render;

/// Explicit worker lifecycle. Rebuild creates fresh backend state/epoch; no detached tasks exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphState {
    /// Accepting PCM/packets and servicing render demand.
    Running,
    /// Draining known FIFO/filter tails without admitting new input.
    Draining,
    /// Finished or explicitly aborted; further input is rejected.
    Stopped,
}
