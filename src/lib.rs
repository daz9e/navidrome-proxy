#![recursion_limit = "256"]
//! navidrome-proxy: an OpenSubsonic proxy that puts an external catalog inside Navidrome.

pub mod config;
pub mod db;
pub mod identity;
pub mod ids;
pub mod matching;
pub mod subsonic;
pub mod tasks;
pub mod util;
