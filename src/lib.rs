//! gh-dash-claude: route an instruction for a GitHub PR or issue to the
//! live Claude Code session that owns it.

pub mod app;
pub mod cli;
pub mod github;
pub mod gitinfo;
pub mod keys;
pub mod message;
pub mod paths;
pub mod proc;
pub mod registry;
pub mod resolve;
pub mod routes;
pub mod shellparse;
pub mod slug;
pub mod target;
pub mod time;
pub mod transcript;
pub mod transport;
pub mod ui;
