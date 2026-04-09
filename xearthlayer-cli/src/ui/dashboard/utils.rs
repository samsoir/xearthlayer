//! Utility functions for the dashboard.
//!
//! This module contains formatting helpers and non-TUI output functions
//! that can be used independently of the terminal UI.

use std::time::Duration;

use xearthlayer::metrics::TelemetrySnapshot;

/// Format duration as HH:MM:SS or MM:SS.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;

    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, mins, secs)
    } else {
        format!("{:02}:{:02}", mins, secs)
    }
}

/// Simple non-TUI fallback for non-interactive terminals.
pub fn print_simple_status(snapshot: &TelemetrySnapshot) {
    println!(
        "[{}] Tiles: {} completed, {} active | Throughput: {} | Cache: {:.0}% mem, {:.0}% dds, {:.0}% chunks",
        snapshot.uptime_human(),
        snapshot.jobs_completed,
        snapshot.jobs_active,
        snapshot.throughput_human(),
        snapshot.memory_cache_hit_rate * 100.0,
        snapshot.dds_disk_cache_hit_rate * 100.0,
        snapshot.chunk_disk_cache_hit_rate * 100.0,
    );
}

/// Print final session summary.
pub fn print_session_summary(snapshot: &TelemetrySnapshot) {
    println!();
    println!("Session Summary");
    println!("───────────────");
    println!(
        "  Tiles generated: {} ({} failed)",
        snapshot.jobs_completed, snapshot.jobs_failed
    );
    println!(
        "  Tiles coalesced: {} ({:.0}% savings)",
        snapshot.jobs_coalesced,
        snapshot.coalescing_rate() * 100.0
    );
    println!("  Data downloaded: {}", snapshot.bytes_downloaded_human());
    println!(
        "  Memory cache: {:.1}% hit rate ({} hits)",
        snapshot.memory_cache_hit_rate * 100.0,
        snapshot.memory_cache_hits
    );
    println!(
        "  DDS disk cache: {:.1}% hit rate ({} hits)",
        snapshot.dds_disk_cache_hit_rate * 100.0,
        snapshot.dds_disk_cache_hits
    );
    println!(
        "  Chunk disk cache: {:.1}% hit rate ({} hits)",
        snapshot.chunk_disk_cache_hit_rate * 100.0,
        snapshot.chunk_disk_cache_hits
    );
    println!("  Avg throughput: {}", snapshot.throughput_human());
    println!("  Uptime: {}", snapshot.uptime_human());
}
