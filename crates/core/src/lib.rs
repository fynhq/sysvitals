//! SysVitals core layer.
//!
//! This crate is the only place where behavior can go wrong: sampling
//! differentials, counter wrap-around, NIC aggregation, unit selection, prefix
//! handling, dual-row layout and history persistence are all logic here so
//! they can be unit tested on any host. The platform display layers are thin
//! glue over these types.
//!
//! Layering contract: no OS-specific windowing or rendering code lives here.
//! `sysinfo` (sampling) and `rusqlite` (history) are the only system
//! touchpoints; each is kept behind its own module ([`sampler`], [`store`])
//! so either can be swapped without changing the public surface.

pub mod backdrop;
pub mod color;
pub mod config;
pub mod format;
pub mod i18n;
pub mod logger;
pub mod model;
pub mod sampler;
pub mod store;

/// Session-local file mapping shared by the Windows indicator and its
/// resident history process. Both processes must use this exact name.
pub const HISTORY_STATE_MAP: &str = "Local\\SysVitalsHistoryState";
/// Layout marker written by the indicator before the history process starts.
pub const HISTORY_STATE_MAGIC: u32 = 0x5348_5653;
/// Bumped whenever [`HistoryState`] changes shape.
pub const HISTORY_STATE_VERSION: u32 = 1;

/// Command/status block mapped by the indicator and the resident history
/// process.
///
/// The indicator writes `desired_open` and `anchor`; the history process
/// writes `visible`. All fields are plain integers/floats so a torn read can
/// at worst place one popup at a stale position — never crash either side.
/// `generation` is bumped after a fresh anchor is written so a reader can
/// retry a write that raced with the next click.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct HistoryState {
    pub magic: u32,
    pub version: u32,
    /// 1 while the user wants the popup open. The history process clears it
    /// when the popup dismisses itself (blur/Esc) so the next click reopens.
    pub desired_open: u32,
    /// 1 from hidden-window creation until the window is removed.
    pub visible: u32,
    /// Incremented by the indicator after writing `anchor`.
    pub generation: u32,
    pub _reserved: u32,
    /// Indicator rect in GPUI global logical pixels (left, top, right,
    /// bottom), the same space the history window is placed in.
    pub anchor: [f32; 4],
}

impl Default for HistoryState {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryState {
    pub const fn new() -> Self {
        Self {
            magic: HISTORY_STATE_MAGIC,
            version: HISTORY_STATE_VERSION,
            desired_open: 0,
            visible: 0,
            generation: 0,
            _reserved: 0,
            anchor: [0.0; 4],
        }
    }

    /// Whether the shared block can be used and carries a non-degenerate
    /// anchor. A malformed block falls back to a centered popup.
    pub fn anchor_is_valid(&self) -> bool {
        self.magic == HISTORY_STATE_MAGIC
            && self.version == HISTORY_STATE_VERSION
            && self.anchor.iter().all(|n| n.is_finite())
            && self.anchor[2] > self.anchor[0]
            && self.anchor[3] > self.anchor[1]
    }
}

pub use config::Config;
pub use format::{format_frame, format_frame_vertical, FormattedRow, Line};
pub use i18n::{t, Language};
pub use model::{Anchor, Metric, Snapshot, Theme};
pub use sampler::{NetSelection, Sampler};
pub use store::{floor_to_grid, spread_label_indices, Bucket, Store};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_state_anchor_validation() {
        let mut state = HistoryState::new();
        assert!(!state.anchor_is_valid(), "zero anchor is degenerate");

        state.anchor = [10.0, 20.0, 110.0, 60.0];
        assert!(state.anchor_is_valid());

        state.anchor[2] = state.anchor[0];
        assert!(!state.anchor_is_valid(), "zero width must be rejected");

        state.anchor = [f32::NAN, 20.0, 110.0, 60.0];
        assert!(
            !state.anchor_is_valid(),
            "non-finite anchor must be rejected"
        );

        state.anchor = [10.0, 20.0, 110.0, 60.0];
        state.version = HISTORY_STATE_VERSION + 1;
        assert!(
            !state.anchor_is_valid(),
            "version mismatch must be rejected"
        );
    }
}
