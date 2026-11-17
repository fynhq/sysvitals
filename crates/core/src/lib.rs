//! Pure core layer: sampling, formatting, config, history, and chart logic; no platform windowing or rendering.

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
pub mod sampler;
pub mod store;

pub use config::Config;
pub use config_watch::{ConfigPoll, ConfigWatcher};
pub use format::{format_frame, format_frame_vertical, FormattedRow, Line};
pub use history_writer::{HistoryWriter, HistoryWriterSlot};
pub use i18n::{t, Language};
pub use model::{Anchor, Metric, Snapshot, Theme};
pub use sampler::Sampler;
pub use store::{floor_to_grid, Bucket, Store};
