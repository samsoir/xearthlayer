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

/// Format the simple non-TUI status line.
///
/// Memory and DDS disk hit rates use the FUSE-only counters so the line
/// reflects X-Plane's actual cache experience; chunks use the aggregate
/// (only one path emits chunk events, so aggregate == FUSE there). See #171.
pub fn format_simple_status(snapshot: &TelemetrySnapshot) -> String {
    format!(
        "[{}] Tiles: {} completed, {} running | Throughput: {} | Cache: {:.0}% mem, {:.0}% dds, {:.0}% chunks",
        snapshot.uptime_human(),
        snapshot.jobs_completed,
        snapshot.jobs_active,
        snapshot.throughput_human(),
        snapshot.fuse_memory_cache_hit_rate * 100.0,
        snapshot.fuse_dds_disk_cache_hit_rate * 100.0,
        snapshot.chunk_disk_cache_hit_rate * 100.0,
    )
}

/// Simple non-TUI fallback for non-interactive terminals.
pub fn print_simple_status(snapshot: &TelemetrySnapshot) {
    println!("{}", format_simple_status(snapshot));
}

/// Format the final session summary as a multi-line string.
///
/// Memory and DDS disk rates use FUSE-only counters (see #171).
pub fn format_session_summary(snapshot: &TelemetrySnapshot) -> String {
    let mut out = String::new();
    out.push('\n');
    out.push_str("Session Summary\n");
    out.push_str("───────────────\n");
    out.push_str(&format!(
        "  Tiles generated: {} ({} failed)\n",
        snapshot.jobs_completed, snapshot.jobs_failed
    ));
    out.push_str(&format!(
        "  Tiles coalesced: {} ({:.0}% savings)\n",
        snapshot.jobs_coalesced,
        snapshot.coalescing_rate() * 100.0
    ));
    out.push_str(&format!(
        "  Data downloaded: {}\n",
        snapshot.bytes_downloaded_human()
    ));
    out.push_str(&format!(
        "  Memory cache: {:.1}% hit rate ({} hits) [FUSE]\n",
        snapshot.fuse_memory_cache_hit_rate * 100.0,
        snapshot.fuse_memory_cache_hits
    ));
    out.push_str(&format!(
        "  DDS disk cache: {:.1}% hit rate ({} hits) [FUSE]\n",
        snapshot.fuse_dds_disk_cache_hit_rate * 100.0,
        snapshot.fuse_dds_disk_cache_hits
    ));
    out.push_str(&format!(
        "  Chunk disk cache: {:.1}% hit rate ({} hits)\n",
        snapshot.chunk_disk_cache_hit_rate * 100.0,
        snapshot.chunk_disk_cache_hits
    ));
    out.push_str(&format!(
        "  Avg throughput: {}\n",
        snapshot.throughput_human()
    ));
    out.push_str(&format!("  Uptime: {}\n", snapshot.uptime_human()));
    out
}

/// Print final session summary.
pub fn print_session_summary(snapshot: &TelemetrySnapshot) {
    print!("{}", format_session_summary(snapshot));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_simple_status_uses_fuse_rates_for_memory_and_dds() {
        // Regression for #171: status line must show FUSE-only hit rates so
        // it matches the TUI cache widget and reflects X-Plane's experience.
        let snapshot = TelemetrySnapshot {
            memory_cache_hit_rate: 0.40,        // aggregate (misleading)
            fuse_memory_cache_hit_rate: 0.90,   // FUSE-only (correct)
            dds_disk_cache_hit_rate: 0.20,      // aggregate
            fuse_dds_disk_cache_hit_rate: 0.80, // FUSE-only
            chunk_disk_cache_hit_rate: 0.05,    // unchanged
            ..Default::default()
        };
        let out = format_simple_status(&snapshot);
        assert!(
            out.contains("90% mem"),
            "expected FUSE memory rate '90% mem', got: {}",
            out
        );
        assert!(
            !out.contains("40% mem"),
            "must not contain aggregate memory rate, got: {}",
            out
        );
        assert!(
            out.contains("80% dds"),
            "expected FUSE DDS rate '80% dds', got: {}",
            out
        );
        assert!(
            !out.contains("20% dds"),
            "must not contain aggregate DDS rate, got: {}",
            out
        );
        assert!(
            out.contains("5% chunks"),
            "chunks should still use aggregate, got: {}",
            out
        );
    }

    #[test]
    fn test_format_session_summary_uses_fuse_rates_for_memory_and_dds() {
        let snapshot = TelemetrySnapshot {
            memory_cache_hits: 1000,
            memory_cache_hit_rate: 0.30,
            fuse_memory_cache_hits: 500,
            fuse_memory_cache_hit_rate: 0.85,
            dds_disk_cache_hits: 800,
            dds_disk_cache_hit_rate: 0.25,
            fuse_dds_disk_cache_hits: 400,
            fuse_dds_disk_cache_hit_rate: 0.75,
            chunk_disk_cache_hits: 50,
            chunk_disk_cache_hit_rate: 0.10,
            ..Default::default()
        };
        let out = format_session_summary(&snapshot);
        assert!(
            out.contains("Memory cache: 85.0% hit rate (500 hits)"),
            "expected FUSE memory line, got:\n{}",
            out
        );
        assert!(
            out.contains("DDS disk cache: 75.0% hit rate (400 hits)"),
            "expected FUSE DDS line, got:\n{}",
            out
        );
        assert!(
            out.contains("Chunk disk cache: 10.0% hit rate (50 hits)"),
            "chunks should use aggregate, got:\n{}",
            out
        );
    }
}
