#![recursion_limit = "256"]
//! navidrome-proxy: an OpenSubsonic proxy that puts an external catalog inside Navidrome.

pub mod admin;
pub mod api;
pub mod app;
pub mod audio;
pub mod catalog;
pub mod config;
pub mod covers;
pub mod db;
pub mod deezer;
pub mod identity;
pub mod ids;
pub mod imports;
pub mod intents;
pub mod library;
pub mod lyrics;
pub mod matching;
pub mod migrate;
pub mod navidrome;
pub mod pipeline;
pub mod process;
pub mod reconcile;
pub mod scheduler;
pub mod slskd;
pub mod sources;
pub mod subsonic;
pub mod tagging;
pub mod tasks;
#[cfg(test)]
pub mod testutil;
pub mod util;
