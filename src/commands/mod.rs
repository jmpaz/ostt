//! Application command handlers for ostt.

pub mod config;
pub mod history;
pub mod keywords;
pub mod list_devices;
pub mod logs;
pub mod record;

pub use config::handle_config;
pub use history::handle_history;
pub use keywords::handle_keywords;
pub use list_devices::handle_list_devices;
pub use logs::handle_logs;
pub use record::{handle_record, RecordingMode};
