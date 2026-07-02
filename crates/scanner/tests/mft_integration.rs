//! Elevated, real-volume integration test for the NTFS $MFT fast path.
//!
//! `#[ignore]`-gated: this test needs an elevated process AND a real NTFS
//! volume, neither of which CI (windows-2022 runners, unelevated) provides.
//! Run it locally, elevated, explicitly:
//!
//!   cargo test -p scanner --test mft_integration -- --ignored --nocapture
//!
//! It scans `C:\` via the MFT path (falling back silently -- and failing the
//! assertion below -- if for any reason the eligibility gate doesn't select
//! it, e.g. not actually elevated), then:
//!   (a) asserts the scan completes and reports `ScanMode::Mft` was used,
//!   (b) compares file count + logical size of a small, low-churn controlled
//!       subtree (this file's own crate directory) against a plain
//!       directory-walk `scan()` of the same subtree, within a small
//!       tolerance (files can change underfoot on a live system),
//!   (c) starts a fresh whole-volume MFT scan and confirms `Cancel` stops it
//!       promptly rather than running to completion.
//!
//! This intentionally duplicates a little channel-draining boilerplate rather
//! than reaching into scanner's private test helpers, since it exercises the
//! crate purely through its public API -- the same way `src-tauri` does.

use scanner::{spawn, Cancel, Record, ScanEvent, ScanMode, ScanOptions};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn drain(opts: ScanOptions) -> (Vec<Record>, Vec<ScanEvent>) {
    let (rec_rx, evt_rx, handle) = spawn(opts, Cancel::new(), 8192, 64);
    let records: Vec<Record> = rec_rx.iter().collect();
    let events: Vec<ScanEvent> = evt_rx.iter().collect();
    let _ = handle.join();
    (records, events)
}

#[test]
#[ignore = "requires an elevated process and a real NTFS C:\\ volume"]
fn mft_scan_of_c_completes_and_reports_mft_mode() {
    let opts = ScanOptions::new(PathBuf::from(r"C:\"));
    let (records, events) = drain(opts);

    assert!(
        events
            .iter()
            .any(|e| matches!(e, ScanEvent::ModeUsed(ScanMode::Mft))),
        "expected the MFT fast path to run when elevated and eligible; got events: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, ScanEvent::Done { .. })),
        "scan should complete with a Done event"
    );
    assert!(
        !records.is_empty(),
        "a whole-volume scan should yield at least the root record"
    );
}

#[test]
#[ignore = "requires an elevated process and a real NTFS C:\\ volume"]
fn mft_scan_matches_walk_on_a_controlled_subtree() {
    // A subtree that exists on any checkout of this repo and is small/stable
    // enough not to churn mid-test: this crate's own `src` directory.
    let subtree = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");

    let mft_opts = ScanOptions::new(subtree.clone());
    let (mft_records, mft_events) = drain(mft_opts);
    assert!(
        mft_events
            .iter()
            .any(|e| matches!(e, ScanEvent::ModeUsed(ScanMode::Mft))),
        "this test only asserts something meaningful if the MFT path actually ran"
    );

    let mut walk_opts = ScanOptions::new(subtree);
    walk_opts.use_mft_fast_path = false; // force the walk for the baseline
    let (walk_records, _walk_events) = drain(walk_opts);

    let mft_files = mft_records
        .iter()
        .filter(|r| r.flags & scanner::FLAG_DIR == 0)
        .count();
    let walk_files = walk_records
        .iter()
        .filter(|r| r.flags & scanner::FLAG_DIR == 0)
        .count();
    let mft_logical: u64 = mft_records.iter().map(|r| r.logical).sum();
    let walk_logical: u64 = walk_records.iter().map(|r| r.logical).sum();

    // Small tolerance: a file could theoretically be created/removed between
    // the two scans on a live system (this subtree is source code, so churn
    // during a local test run is unlikely but not impossible).
    let file_diff = (mft_files as i64 - walk_files as i64).unsigned_abs();
    assert!(
        file_diff <= 2,
        "file count mismatch: mft={mft_files} walk={walk_files}"
    );
    let logical_diff = mft_logical.abs_diff(walk_logical);
    let tolerance = walk_logical / 100 + 4096; // 1% + a cluster of slack
    assert!(
        logical_diff <= tolerance,
        "logical size mismatch: mft={mft_logical} walk={walk_logical} (tolerance {tolerance})"
    );
}

#[test]
#[ignore = "requires an elevated process and a real NTFS C:\\ volume"]
fn cancel_stops_an_in_progress_mft_scan_promptly() {
    let opts = ScanOptions::new(PathBuf::from(r"C:\"));
    let cancel = Cancel::new();
    let (rec_rx, evt_rx, handle) = spawn(opts, cancel.clone(), 8192, 64);

    // Let it get underway, then cancel and require prompt shutdown.
    std::thread::sleep(Duration::from_millis(200));
    cancel.cancel();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_cancelled = false;
    while Instant::now() < deadline {
        // Keep draining records too, so the scanner threads (which may be
        // blocked on a full bounded record channel) can notice the
        // cancellation and exit rather than deadlocking this test.
        while rec_rx.try_recv().is_ok() {}
        match evt_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ScanEvent::Cancelled) => {
                saw_cancelled = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    while rec_rx.try_recv().is_ok() {}
    let _ = handle.join();

    assert!(
        saw_cancelled,
        "expected a Cancelled event within 30s of calling cancel() on a whole-volume MFT scan"
    );
}
