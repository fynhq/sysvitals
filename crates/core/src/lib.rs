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
/// Ownership:
/// - the indicator owns `desired_open`, `anchor` and `generation`;
/// - the history process owns `visible`.
///
/// `generation` is both the request sequence and a seqlock: the indicator
/// writes it odd, updates `desired_open`/`anchor`, then writes it even.
/// Readers take a snapshot, re-read the generation and retry when it changed
/// or is odd, so a popup can never be placed with a half-updated anchor.
/// Because the history process never writes `desired_open`, a self-dismissed
/// popup cannot swallow a click that arrived while it was closing; the
/// `visible` flag is the actual state the indicator toggles on.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct HistoryState {
    pub magic: u32,
    pub version: u32,
    /// 1 while the user wants the popup open (indicator-owned).
    pub desired_open: u32,
    /// 1 from hidden-window creation until the window is removed
    /// (history-process-owned).
    pub visible: u32,
    /// Indicator-owned request sequence; also the anchor seqlock.
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

    /// Read a consistent snapshot from a mapped block.
    ///
    /// This is a seqlock read: `generation` is odd while the indicator is
    /// writing the request, so the caller retries instead of consuming a torn
    /// anchor. `visible` is written independently by the history process; a
    /// single 32-bit load is enough for that status flag.
    ///
    /// The retry count is bounded so a writer that died mid-update cannot
    /// wedge the history process forever. After that the best-effort read is
    /// returned; `anchor_is_valid` still filters degenerate rectangles.
    ///
    /// # Safety
    /// `ptr` must point to a live, properly aligned `HistoryState` mapping
    /// shared with the other process.
    pub unsafe fn read_consistent(ptr: *const HistoryState) -> HistoryState {
        use std::ptr::{addr_of, read_volatile};
        let read = |field: *const u32| unsafe { read_volatile(field) };
        for _ in 0..1024 {
            let g1 = read(addr_of!((*ptr).generation));
            if g1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let state = HistoryState {
                magic: read(addr_of!((*ptr).magic)),
                version: read(addr_of!((*ptr).version)),
                desired_open: read(addr_of!((*ptr).desired_open)),
                visible: read(addr_of!((*ptr).visible)),
                generation: g1,
                _reserved: 0,
                anchor: [
                    read_volatile(addr_of!((*ptr).anchor[0])),
                    read_volatile(addr_of!((*ptr).anchor[1])),
                    read_volatile(addr_of!((*ptr).anchor[2])),
                    read_volatile(addr_of!((*ptr).anchor[3])),
                ],
            };
            if read(addr_of!((*ptr).generation)) == g1 {
                return state;
            }
            std::hint::spin_loop();
        }
        // Best effort after a stalled writer. The fields are still finite or
        // rejected by `anchor_is_valid` at the call site.
        HistoryState {
            magic: read(addr_of!((*ptr).magic)),
            version: read(addr_of!((*ptr).version)),
            desired_open: read(addr_of!((*ptr).desired_open)),
            visible: read(addr_of!((*ptr).visible)),
            generation: read(addr_of!((*ptr).generation)),
            _reserved: 0,
            anchor: [
                read_volatile(addr_of!((*ptr).anchor[0])),
                read_volatile(addr_of!((*ptr).anchor[1])),
                read_volatile(addr_of!((*ptr).anchor[2])),
                read_volatile(addr_of!((*ptr).anchor[3])),
            ],
        }
    }

    /// Publish one indicator request (`desired_open` plus an optional anchor)
    /// under the generation seqlock.
    ///
    /// # Safety
    /// `ptr` must point to a live, properly aligned `HistoryState` mapping
    /// shared with the other process, and only the indicator may call this.
    pub unsafe fn write_request(
        ptr: *mut HistoryState,
        desired_open: bool,
        anchor: Option<[f32; 4]>,
    ) {
        use std::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
        use std::sync::atomic::{fence, Ordering};
        let current = read_volatile(addr_of!((*ptr).generation));
        let odd = current.wrapping_add(1) | 1;
        write_volatile(addr_of_mut!((*ptr).generation), odd);
        fence(Ordering::SeqCst);
        if let Some(anchor) = anchor {
            for (slot, value) in anchor.iter().enumerate() {
                write_volatile(addr_of_mut!((*ptr).anchor[slot]), *value);
            }
        }
        write_volatile(addr_of_mut!((*ptr).desired_open), u32::from(desired_open));
        fence(Ordering::SeqCst);
        write_volatile(addr_of_mut!((*ptr).generation), odd.wrapping_add(1));
    }

    /// Publish the history process's actual visibility. This flag is
    /// single-writer, so it does not participate in the seqlock.
    ///
    /// # Safety
    /// `ptr` must point to a live, properly aligned `HistoryState` mapping
    /// shared with the other process, and only the history process may call
    /// this.
    pub unsafe fn write_visible(ptr: *mut HistoryState, visible: bool) {
        use std::ptr::{addr_of_mut, write_volatile};
        write_volatile(addr_of_mut!((*ptr).visible), u32::from(visible));
    }

    /// Clear both flags after the history process died: a stale `visible`
    /// would otherwise make the next click a no-op toggle-close.
    ///
    /// # Safety
    /// Same contract as [`HistoryState::write_request`].
    pub unsafe fn clear(ptr: *mut HistoryState) {
        unsafe {
            Self::write_request(ptr, false, None);
            Self::write_visible(ptr, false);
        }
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

    #[test]
    fn history_state_request_roundtrip_uses_the_seqlock() {
        let mut state = HistoryState::new();
        let ptr = &mut state as *mut HistoryState;
        unsafe {
            HistoryState::write_request(ptr, true, Some([10.0, 20.0, 110.0, 60.0]));
            let read = HistoryState::read_consistent(ptr);
            assert_eq!(read.generation & 1, 0, "a published write is even");
            assert_eq!(read.desired_open, 1);
            assert_eq!(read.anchor, [10.0, 20.0, 110.0, 60.0]);
            assert!(read.anchor_is_valid());

            HistoryState::write_visible(ptr, true);
            let read = HistoryState::read_consistent(ptr);
            assert_eq!(read.visible, 1);
            assert_eq!(read.generation, state.generation);

            HistoryState::write_request(ptr, false, None);
            let read = HistoryState::read_consistent(ptr);
            assert_eq!(read.desired_open, 0);
            assert_eq!(read.visible, 1, "a close request leaves visibility alone");

            HistoryState::clear(ptr);
            let read = HistoryState::read_consistent(ptr);
            assert_eq!(read.desired_open, 0);
            assert_eq!(read.visible, 0);
        }
    }
}
