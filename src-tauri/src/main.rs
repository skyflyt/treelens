// Prevents additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `treelens --selftest` exercises the file-op pipeline (create folder →
    // create file → rename → recycle) against a temp scratch dir and exits with
    // 0/1. It's how we smoke-test the destructive ops without driving the
    // WebView (synthesized clicks don't reach WebView2's JS handlers). Harmless
    // in normal use — only runs with the explicit flag.
    if std::env::args().any(|a| a == "--selftest") {
        std::process::exit(treelens_lib::selftest());
    }
    treelens_lib::run();
}
