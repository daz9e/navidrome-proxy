//! The command line: JSON output, the owner lock and refusal messages.

use navidrome_proxy::db::{OwnerLock, SCHEMA_VERSION, Store};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Output;

fn bin() -> String {
    // rt.sh runs binaries built on another host; it points NDP_BIN at the copied binary.
    std::env::var("NDP_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_navidrome-proxy").to_string())
}

fn run(dir: &Path, args: &[&str]) -> Output {
    std::fs::create_dir_all(dir.join("music")).unwrap();
    std::process::Command::new(bin())
        .args(args)
        .env_clear()
        .env("MUSIC_DIR", dir.join("music"))
        .env("DATA_DIR", dir.join("data"))
        .env("LOG_LEVEL", "ERROR")
        .output()
        .unwrap()
}

fn json_of(out: Output) -> Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn refused(out: Output) -> String {
    assert!(!out.status.success());
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn check_db_and_relayout_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(json_of(run(dir.path(), &["check-db"])), json!({"ok": true, "problems": []}));
    assert_eq!(json_of(run(dir.path(), &["relayout", "--dry-run"])), json!({"moved": 0, "kept": 0, "missing": 0, "skipped": 0}));
}

#[test]
fn remux_dash_needs_no_database() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(json_of(run(dir.path(), &["remux-dash", "--dry-run"]))["checked"], 0);
    assert!(!dir.path().join("data/proxy.sqlite3").exists());
}

#[test]
fn a_second_process_is_refused_and_the_lock_is_released_after_a_command() {
    let dir = tempfile::tempdir().unwrap();
    json_of(run(dir.path(), &["check-db"]));
    let mut lock = OwnerLock::new(&dir.path().join("data/proxy.sqlite3"));
    lock.acquire().unwrap();
    assert!(refused(run(dir.path(), &["check-db"])).contains("held by another"));
    lock.release();
}

#[test]
fn a_newer_database_is_refused_with_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data/proxy.sqlite3")).unwrap();
    store.set_meta("schema_version", &(SCHEMA_VERSION + 1).to_string());
    drop(store);
    assert!(refused(run(dir.path(), &["check-db"])).contains("schema version"));
}

#[test]
fn recheck_of_an_unknown_import() {
    let dir = tempfile::tempdir().unwrap();
    assert!(refused(run(dir.path(), &["recheck-import", "nope"])).contains("no import nope"));
}
