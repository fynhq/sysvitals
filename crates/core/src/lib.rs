//! Pure core layer: sampling, formatting, config, history, and shared memory; no platform windowing or rendering.

pub mod backdrop;
pub mod chart;
pub mod cli;
pub mod color;
pub mod config;
pub mod config_watch;
pub mod format;
pub mod history_writer;
pub mod i18n;
pub mod layout;
pub mod logger;
pub mod model;
pub mod resident;
pub mod sampler;
pub mod store;

pub const HISTORY_STATE_MAP: &str = "Local\\SysVitalsHistoryState";
pub const HISTORY_STATE_MAGIC: u32 = 0x5348_5653;
pub const HISTORY_STATE_VERSION: u32 = 3;

// Indicator owns desired_open/anchor/monitor/generation; history owns visible/closed_generation; generation is the seqlock.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct HistoryState {
    pub magic: u32,
    pub version: u32,
    pub desired_open: u32,
    pub visible: u32,
    pub generation: u32,
    pub closed_generation: u32,
    pub _reserved: u32,
    pub anchor: [f32; 4],
    pub monitor: i64,
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
            closed_generation: 0,
            _reserved: 0,
            anchor: [0.0; 4],
            monitor: 0,
        }
    }

    pub fn anchor_is_valid(&self) -> bool {
        self.magic == HISTORY_STATE_MAGIC
            && self.version == HISTORY_STATE_VERSION
            && self.anchor.iter().all(|n| n.is_finite())
            && self.anchor[2] > self.anchor[0]
            && self.anchor[3] > self.anchor[1]
    }

    // A clean idle exit records closed_generation; a newer generation means a click raced shutdown and must relaunch.
    pub fn click_raced_idle_exit(&self, clean_exit: bool) -> bool {
        self.desired_open != 0
            && clean_exit
            && self.closed_generation != 0
            && self.generation != self.closed_generation
    }

    /// Seqlock read: retry on odd/changed generation, bounded so a dead writer cannot wedge the reader.
    ///
    /// # Safety
    /// `ptr` must point to a live, aligned `HistoryState` mapping shared with the other process.
    pub unsafe fn read_consistent(ptr: *const HistoryState) -> HistoryState {
        use std::ptr::{addr_of, read_volatile};
        use std::sync::atomic::{fence, Ordering};
        let read = |field: *const u32| unsafe { read_volatile(field) };
        for _ in 0..1024 {
            let g1 = read(addr_of!((*ptr).generation));
            if g1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            fence(Ordering::Acquire);
            let state = HistoryState {
                magic: read(addr_of!((*ptr).magic)),
                version: read(addr_of!((*ptr).version)),
                desired_open: read(addr_of!((*ptr).desired_open)),
                visible: read(addr_of!((*ptr).visible)),
                generation: g1,
                closed_generation: read(addr_of!((*ptr).closed_generation)),
                _reserved: 0,
                anchor: [
                    read_volatile(addr_of!((*ptr).anchor[0])),
                    read_volatile(addr_of!((*ptr).anchor[1])),
                    read_volatile(addr_of!((*ptr).anchor[2])),
                    read_volatile(addr_of!((*ptr).anchor[3])),
                ],
                monitor: read_volatile(addr_of!((*ptr).monitor)),
            };
            fence(Ordering::Acquire);
            if read(addr_of!((*ptr).generation)) == g1 {
                return state;
            }
            std::hint::spin_loop();
        }
        fence(Ordering::Acquire);
        HistoryState {
            magic: read(addr_of!((*ptr).magic)),
            version: read(addr_of!((*ptr).version)),
            desired_open: read(addr_of!((*ptr).desired_open)),
            visible: read(addr_of!((*ptr).visible)),
            generation: read(addr_of!((*ptr).generation)),
            closed_generation: read(addr_of!((*ptr).closed_generation)),
            _reserved: 0,
            anchor: [
                read_volatile(addr_of!((*ptr).anchor[0])),
                read_volatile(addr_of!((*ptr).anchor[1])),
                read_volatile(addr_of!((*ptr).anchor[2])),
                read_volatile(addr_of!((*ptr).anchor[3])),
            ],
            monitor: read_volatile(addr_of!((*ptr).monitor)),
        }
    }

    /// # Safety
    /// `ptr` must point to a live, aligned `HistoryState` mapping shared with the other process; only the indicator may write.
    pub unsafe fn write_request(
        ptr: *mut HistoryState,
        desired_open: bool,
        anchor: Option<[f32; 4]>,
        monitor: i64,
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
        write_volatile(addr_of_mut!((*ptr).monitor), monitor);
        write_volatile(addr_of_mut!((*ptr).desired_open), u32::from(desired_open));
        fence(Ordering::SeqCst);
        write_volatile(addr_of_mut!((*ptr).generation), odd.wrapping_add(1));
    }

    /// # Safety
    /// `ptr` must point to a live, aligned `HistoryState` mapping shared with the other process; only the history process may write.
    pub unsafe fn write_visible(ptr: *mut HistoryState, visible: bool) {
        use std::ptr::{addr_of_mut, write_volatile};
        write_volatile(addr_of_mut!((*ptr).visible), u32::from(visible));
    }

    /// # Safety
    /// `ptr` must point to a live, aligned `HistoryState` mapping shared with the other process; only the history process may write.
    pub unsafe fn write_closed_generation(ptr: *mut HistoryState, generation: u32) {
        use std::ptr::{addr_of_mut, write_volatile};
        write_volatile(addr_of_mut!((*ptr).closed_generation), generation);
    }

    /// Clear both flags after a crash; a stale visible would make the next click a no-op toggle-close.
    ///
    /// # Safety
    /// Same contract as `write_request`.
    pub unsafe fn clear(ptr: *mut HistoryState) {
        unsafe {
            Self::write_request(ptr, false, None, 0);
            Self::write_visible(ptr, false);
            Self::write_closed_generation(ptr, 0);
        }
    }
}

pub use config::Config;
pub use config_watch::{ConfigPoll, ConfigWatcher};
pub use format::{format_frame, format_frame_vertical, FormattedRow, Line};
pub use history_writer::{HistoryWriter, HistoryWriterSlot};
pub use i18n::{t, Language};
pub use model::{Anchor, Metric, Snapshot, Theme};
pub use sampler::Sampler;
pub use store::{floor_to_grid, Bucket, Store};

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
    fn only_a_newer_click_after_a_clean_idle_exit_is_relaunched() {
        let mut state = HistoryState::new();
        state.desired_open = 1;
        state.generation = 10;
        state.closed_generation = 8;
        assert!(state.click_raced_idle_exit(true));
        assert!(
            !state.click_raced_idle_exit(false),
            "a crash must not relaunch"
        );

        state.closed_generation = 10;
        assert!(
            !state.click_raced_idle_exit(true),
            "the consumed request is not a race"
        );

        state.closed_generation = 0;
        assert!(
            !state.click_raced_idle_exit(true),
            "no idle-exit handshake means no relaunch"
        );

        state.closed_generation = 8;
        state.desired_open = 0;
        assert!(
            !state.click_raced_idle_exit(true),
            "there is no pending click to preserve"
        );
    }

    #[test]
    fn history_state_request_roundtrip_uses_the_seqlock() {
        let mut state = HistoryState::new();
        let ptr = &mut state as *mut HistoryState;
        let monitor = 0x0000_0000_0001_2345i64;
        unsafe {
            HistoryState::write_request(ptr, true, Some([10.0, 20.0, 110.0, 60.0]), monitor);
            let read = HistoryState::read_consistent(ptr);
            assert_eq!(read.generation & 1, 0, "a published write is even");
            assert_eq!(read.desired_open, 1);
            assert_eq!(read.anchor, [10.0, 20.0, 110.0, 60.0]);
            assert_eq!(read.monitor, monitor);
            assert!(read.anchor_is_valid());

            HistoryState::write_visible(ptr, true);
            let read = HistoryState::read_consistent(ptr);
            assert_eq!(read.visible, 1);
            assert_eq!(read.generation, state.generation);

            HistoryState::write_request(ptr, false, None, 0);
            let read = HistoryState::read_consistent(ptr);
            assert_eq!(read.desired_open, 0);
            assert_eq!(read.monitor, 0, "a close request drops the display hint");
            assert_eq!(read.visible, 1, "a close request leaves visibility alone");

            HistoryState::write_closed_generation(ptr, read.generation);
            let read = HistoryState::read_consistent(ptr);
            assert_eq!(
                read.closed_generation, read.generation,
                "an idle exit records the request it consumed"
            );

            HistoryState::clear(ptr);
            let read = HistoryState::read_consistent(ptr);
            assert_eq!(read.desired_open, 0);
            assert_eq!(read.visible, 0);
            assert_eq!(read.monitor, 0);
            assert_eq!(
                read.closed_generation, 0,
                "clearing a dead resident resets the handshake"
            );
        }
    }
}
