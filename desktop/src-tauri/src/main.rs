// Desktop binary entry point. All logic lives in `lib.rs` so the same
// code can be built as a cdylib for Android/iOS via `cargo tauri android build`.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    wzp_desktop_lib::run();
}
