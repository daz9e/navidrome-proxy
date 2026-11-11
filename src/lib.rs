#![recursion_limit = "256"]
//! navidrome-proxy: an OpenSubsonic proxy that puts an external catalog inside Navidrome.

pub mod config;
pub mod covers;
pub mod db;
pub mod deezer;
pub mod identity;
pub mod ids;
pub mod lyrics;
pub mod matching;
pub mod navidrome;
pub mod subsonic;
pub mod tasks;
#[cfg(test)]
pub mod testutil;
pub mod util;
