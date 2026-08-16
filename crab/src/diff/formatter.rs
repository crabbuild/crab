//! Output rendering: human / JSON / stat / name-only formatting.

use std::io::Write;

use crab_diff::types::{
    ChunkDiffMetrics, DiffSummary, FileDiffEntry, FileStatus, OutputMode, SegmentStatus,
};

// ANSI color codes — applied conditionally, no external dependency needed.
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Format a byte count into a human-friendly size string.
///
/// Uses B (no decimal) for values < 1024, then KB/MB/GB/TB with one
/// decimal place.
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    match bytes {
        0..KB => format!("{bytes} B"),
        KB..MB => format!("{:.1} KB", bytes as f64 / KB as f64),
        MB..GB => format!("{:.1} MB", bytes as f64 / MB as f64),
        GB..TB => format!("{:.1} GB", bytes as f64 / GB as f64),
        _ => format!("{:.1} TB", bytes as f64 / TB as f64),
    }
}

/// Render diff results to the given writer.
///
/// Reports are sorted by file path before rendering regardless of mode.
/// The `show_byte_ranges` flag controls whether changed byte offset
/// ranges are printed under each file in human-readable modes.
pub fn format_diff(
    reports: &[FileDiffEntry],
    summary: &DiffSummary,
    mode: OutputMode,
    color: bool,
    show_byte_ranges: bool,
    writer: &mut dyn Write,
) -> std::io::Result<()> {
    let mut sorted: Vec<&FileDiffEntry> = reports.iter().collect();
    sorted.sort_by(|a, b| a.report.path.cmp(&b.report.path));

    match mode {
        OutputMode::Json => format_json(&sorted, summary, writer),
        OutputMode::NameOnly => format_name_only(&sorted, writer),
        OutputMode::Stat => format_stat(&sorted, summary, color, writer),
        OutputMode::Human => format_human(&sorted, summary, color, show_byte_ranges, false, writer),
        OutputMode::HumanVerbose => {
            format_human(&sorted, summary, color, show_byte_ranges, true, writer)
        }
    }
}

/// JSON output: `{ "files": [...], "summary": {...} }`.
fn format_json(
    sorted: &[&FileDiffEntry],
    summary: &DiffSummary,
    writer: &mut dyn Write,
) -> std::io::Result<()> {
    #[derive(serde::Serialize)]
    struct JsonOutput<'a> {
        files: Vec<&'a FileDiffEntry>,
        summary: &'a DiffSummary,
    }
    let output = JsonOutput {
        files: sorted.to_vec(),
        summary,
    };
    serde_json::to_writer_pretty(writer, &output).map_err(std::io::Error::other)?;
    Ok(())
}

/// Name-only: one path per line.
fn format_name_only(sorted: &[&FileDiffEntry], writer: &mut dyn Write) -> std::io::Result<()> {
    for entry in sorted {
        writeln!(writer, "{}", entry.report.path)?;
    }
    Ok(())
}

/// Summary line: `<N> files changed, <M> chunks changed, <delta> delta`.
fn format_summary(
    summary: &DiffSummary,
    display_delta_bytes: u64,
    color: bool,
    writer: &mut dyn Write,
) -> std::io::Result<()> {
    let delta = format_size(display_delta_bytes);
    if color {
        writeln!(
            writer,
            "{YELLOW}{} files changed{RESET}, {} chunks changed, {} delta",
            summary.files_changed, summary.total_segments_changed, delta,
        )
    } else {
        writeln!(
            writer,
            "{} files changed, {} chunks changed, {} delta",
            summary.files_changed, summary.total_segments_changed, delta,
        )
    }
}

/// Stat output: one compact line per file plus the aggregate summary.
fn format_stat(
    sorted: &[&FileDiffEntry],
    summary: &DiffSummary,
    color: bool,
    writer: &mut dyn Write,
) -> std::io::Result<()> {
    for entry in sorted {
        let r = &entry.report;
        if let Some(metrics) = &r.chunk_metrics {
            format_large_file_stat(r.path.as_str(), r.status, metrics, color, writer)?;
            continue;
        }

        let changed = r.added_segments + r.removed_segments;
        let delta = format_size(r.delta_bytes);
        if color {
            writeln!(
                writer,
                "{YELLOW}{}: {changed} chunks changed ({delta} delta, {:.1}% dedup){RESET}",
                r.path,
                r.dedup_ratio * 100.0
            )?;
        } else {
            writeln!(
                writer,
                "{}: {changed} chunks changed ({delta} delta, {:.1}% dedup)",
                r.path,
                r.dedup_ratio * 100.0
            )?;
        }
    }

    if !sorted.is_empty() {
        writeln!(writer)?;
    }
    format_summary(
        summary,
        display_total_delta_bytes(sorted, summary),
        color,
        writer,
    )
}

fn format_large_file_stat(
    path: &str,
    status: FileStatus,
    metrics: &ChunkDiffMetrics,
    color: bool,
    writer: &mut dyn Write,
) -> std::io::Result<()> {
    let changed = metrics.added_chunks + metrics.removed_chunks;
    let delta = format_signed_size(metrics.signed_delta_bytes);
    let reuse = metrics.reuse_ratio * 100.0;
    let line = match status {
        FileStatus::Added => format!(
            "{path}: added {}, {changed} chunks changed ({delta} size delta, {reuse:.1}% reuse)",
            format_size(metrics.new_bytes)
        ),
        FileStatus::Deleted => format!(
            "{path}: deleted {}, {changed} chunks changed ({delta} size delta, {reuse:.1}% reuse)",
            format_size(metrics.old_bytes)
        ),
        FileStatus::Modified => format!(
            "{path}: {} -> {}, {changed} chunks changed ({delta} size delta, {reuse:.1}% reuse)",
            format_size(metrics.old_bytes),
            format_size(metrics.new_bytes)
        ),
        FileStatus::GitNative => format!("{path}: git-native (chunk diff unavailable)"),
    };

    if color {
        writeln!(writer, "{YELLOW}{line}{RESET}")
    } else {
        writeln!(writer, "{line}")
    }
}

fn format_signed_size(delta: i64) -> String {
    match delta.cmp(&0) {
        std::cmp::Ordering::Greater => format!("+{}", format_size(delta as u64)),
        std::cmp::Ordering::Less => format!("-{}", format_size(delta.unsigned_abs())),
        std::cmp::Ordering::Equal => "0 B".to_owned(),
    }
}

/// Human-readable output with optional verbose segment details.
fn format_human(
    sorted: &[&FileDiffEntry],
    summary: &DiffSummary,
    color: bool,
    show_byte_ranges: bool,
    verbose: bool,
    writer: &mut dyn Write,
) -> std::io::Result<()> {
    for entry in sorted {
        let r = &entry.report;
        match r.status {
            FileStatus::Added => {
                let size = format_size(r.new_size);
                let chunks = r
                    .chunk_metrics
                    .as_ref()
                    .map_or(r.added_segments, |metrics| metrics.new_chunks);
                if color {
                    writeln!(
                        writer,
                        "{GREEN}{}: added ({size}, {chunks} chunks){RESET}",
                        r.path
                    )?;
                } else {
                    writeln!(writer, "{}: added ({size}, {chunks} chunks)", r.path)?;
                }
            }
            FileStatus::Deleted => {
                let size = format_size(r.old_size);
                let chunks = r
                    .chunk_metrics
                    .as_ref()
                    .map_or(r.removed_segments, |metrics| metrics.old_chunks);
                if color {
                    writeln!(
                        writer,
                        "{RED}{}: deleted ({size}, {chunks} chunks){RESET}",
                        r.path
                    )?;
                } else {
                    writeln!(writer, "{}: deleted ({size}, {chunks} chunks)", r.path)?;
                }
            }
            FileStatus::Modified => {
                let old = format_size(r.old_size);
                let new = format_size(r.new_size);
                let changed = r
                    .chunk_metrics
                    .as_ref()
                    .map_or(r.added_segments + r.removed_segments, |metrics| {
                        metrics.added_chunks + metrics.removed_chunks
                    });
                let (delta, ratio_label) = if let Some(metrics) = &r.chunk_metrics {
                    (
                        format!(
                            "{} size delta",
                            format_signed_size(metrics.signed_delta_bytes)
                        ),
                        "reuse",
                    )
                } else {
                    (format!("{} delta", format_size(r.delta_bytes)), "dedup")
                };
                let ratio = format!("{:.1}", r.dedup_ratio * 100.0);
                if color {
                    writeln!(
                        writer,
                        "{YELLOW}{}: {old} → {new}, {changed} chunks changed ({delta}, {ratio}% {ratio_label}){RESET}",
                        r.path,
                    )?;
                } else {
                    writeln!(
                        writer,
                        "{}: {old} → {new}, {changed} chunks changed ({delta}, {ratio}% {ratio_label})",
                        r.path,
                    )?;
                }
            }
            FileStatus::GitNative => {
                if color {
                    writeln!(
                        writer,
                        "{DIM}{}: git-native (chunk diff unavailable){RESET}",
                        r.path
                    )?;
                } else {
                    writeln!(writer, "{}: git-native (chunk diff unavailable)", r.path)?;
                }
            }
        }

        // Verbose segment details (only for Human/HumanVerbose, not GitNative).
        if verbose && r.status != FileStatus::GitNative {
            for seg in &r.segment_details {
                let hash_str = match seg.status {
                    SegmentStatus::Added => seg
                        .new_xorb_hash
                        .as_deref()
                        .map(truncate_hash)
                        .unwrap_or_default(),
                    SegmentStatus::Removed | SegmentStatus::Unchanged => seg
                        .old_xorb_hash
                        .as_deref()
                        .map(truncate_hash)
                        .unwrap_or_default(),
                };
                let range = match seg.status {
                    SegmentStatus::Added => seg.new_chunk_range,
                    SegmentStatus::Removed | SegmentStatus::Unchanged => seg.old_chunk_range,
                };
                let range_str = range
                    .map(|(s, e)| format!("[{s}..{e}]"))
                    .unwrap_or_default();
                let size = format_size(seg.bytes);
                let status_str = match seg.status {
                    SegmentStatus::Unchanged => "unchanged",
                    SegmentStatus::Added => "added",
                    SegmentStatus::Removed => "removed",
                };
                let line = format!(
                    "  segment[{}]: xorb {hash_str} {range_str} ({size}) {status_str}",
                    seg.index,
                );
                if color {
                    match seg.status {
                        SegmentStatus::Added => writeln!(writer, "{GREEN}{line}{RESET}")?,
                        SegmentStatus::Removed => writeln!(writer, "{RED}{line}{RESET}")?,
                        SegmentStatus::Unchanged => writeln!(writer, "{DIM}{line}{RESET}")?,
                    }
                } else {
                    writeln!(writer, "{line}")?;
                }
            }
        }

        // Byte ranges.
        if show_byte_ranges && r.status != FileStatus::GitNative {
            for &(offset, length) in &r.changed_byte_ranges {
                let end = offset + length;
                writeln!(writer, "  bytes {offset}\u{2013}{end} changed")?;
            }
        }

        // Annotations (indented under the file line).
        for ann in &r.annotations {
            writeln!(writer, "  {ann}")?;
        }
    }

    // Blank line before summary when there are file entries.
    if !sorted.is_empty() {
        writeln!(writer)?;
    }
    format_summary(
        summary,
        display_total_delta_bytes(sorted, summary),
        color,
        writer,
    )
}

fn display_total_delta_bytes(sorted: &[&FileDiffEntry], summary: &DiffSummary) -> u64 {
    if sorted.is_empty() {
        return summary.total_delta_bytes;
    }

    sorted
        .iter()
        .filter(|entry| entry.report.status != FileStatus::GitNative)
        .map(|entry| {
            entry
                .report
                .chunk_metrics
                .as_ref()
                .map_or(entry.report.delta_bytes, |metrics| {
                    metrics.signed_delta_bytes.unsigned_abs()
                })
        })
        .sum()
}

/// Truncate a hex hash to 6 chars followed by `..`.
fn truncate_hash(hash: &str) -> String {
    if hash.len() > 6 {
        format!("{}..", &hash[..6])
    } else {
        format!("{hash}..")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_diff::types::{ChunkDiffReport, ChunkSequenceSourceKind, SegmentDiff};

    fn make_modified_report(path: &str) -> FileDiffEntry {
        FileDiffEntry {
            report: ChunkDiffReport {
                path: path.to_string(),
                status: FileStatus::Modified,
                old_size: 10_737_418_240,
                new_size: 10_737_418_240,
                unchanged_segments: 157,
                unchanged_bytes: 10_690_232_320,
                removed_segments: 3,
                removed_bytes: 47_185_920,
                added_segments: 3,
                added_bytes: 47_185_920,
                delta_bytes: 47_185_920,
                dedup_ratio: 0.9956,
                changed_byte_ranges: vec![(4_521_984_000, 47_185_920)],
                segment_details: vec![],
                annotations: vec![
                    "tensor layers.23.attention.weight: 12.0 MB modified".to_string(),
                ],
                chunk_metrics: None,
            },
        }
    }

    fn make_chunk_metric_report(path: &str) -> FileDiffEntry {
        let mut entry = make_modified_report(path);
        entry.report.old_size = 100;
        entry.report.new_size = 120;
        entry.report.unchanged_segments = 2;
        entry.report.removed_segments = 1;
        entry.report.added_segments = 2;
        entry.report.unchanged_bytes = 80;
        entry.report.removed_bytes = 20;
        entry.report.added_bytes = 40;
        entry.report.delta_bytes = 40;
        entry.report.dedup_ratio = 80.0 / 120.0;
        entry.report.chunk_metrics = Some(ChunkDiffMetrics {
            old_source: ChunkSequenceSourceKind::Committed,
            new_source: ChunkSequenceSourceKind::Staged,
            old_chunks: 3,
            new_chunks: 4,
            unchanged_chunks: 2,
            removed_chunks: 1,
            added_chunks: 2,
            old_bytes: 100,
            new_bytes: 120,
            unchanged_bytes: 80,
            removed_bytes: 20,
            added_bytes: 40,
            signed_delta_bytes: 20,
            reuse_ratio: 80.0 / 120.0,
            changed_byte_ranges_old: vec![(80, 20)],
            changed_byte_ranges_new: vec![(80, 40)],
        });
        entry
    }

    fn make_added_report(path: &str) -> FileDiffEntry {
        FileDiffEntry {
            report: ChunkDiffReport {
                path: path.to_string(),
                status: FileStatus::Added,
                old_size: 0,
                new_size: 1228,
                unchanged_segments: 0,
                unchanged_bytes: 0,
                removed_segments: 0,
                removed_bytes: 0,
                added_segments: 1,
                added_bytes: 1228,
                delta_bytes: 1228,
                dedup_ratio: 0.0,
                changed_byte_ranges: vec![],
                segment_details: vec![],
                annotations: vec![],
                chunk_metrics: None,
            },
        }
    }

    fn make_deleted_report(path: &str) -> FileDiffEntry {
        FileDiffEntry {
            report: ChunkDiffReport {
                path: path.to_string(),
                status: FileStatus::Deleted,
                old_size: 2048,
                new_size: 0,
                unchanged_segments: 0,
                unchanged_bytes: 0,
                removed_segments: 2,
                removed_bytes: 2048,
                added_segments: 0,
                added_bytes: 0,
                delta_bytes: 0,
                dedup_ratio: 0.0,
                changed_byte_ranges: vec![],
                segment_details: vec![],
                annotations: vec![],
                chunk_metrics: None,
            },
        }
    }

    fn make_summary() -> DiffSummary {
        DiffSummary {
            files_changed: 3,
            total_segments_changed: 6,
            total_delta_bytes: 47_187_148,
        }
    }

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn format_size_megabytes() {
        assert_eq!(format_size(1_048_576), "1.0 MB");
        assert_eq!(format_size(47_185_920), "45.0 MB");
    }

    #[test]
    fn format_size_gigabytes() {
        assert_eq!(format_size(1_073_741_824), "1.0 GB");
        assert_eq!(format_size(10_737_418_240), "10.0 GB");
    }

    #[test]
    fn format_size_terabytes() {
        assert_eq!(format_size(1_099_511_627_776), "1.0 TB");
    }

    #[test]
    fn name_only_output() {
        let reports = vec![
            make_added_report("z_file.txt"),
            make_modified_report("a_model.safetensors"),
        ];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(
            &reports,
            &summary,
            OutputMode::NameOnly,
            false,
            false,
            &mut buf,
        )
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "a_model.safetensors");
        assert_eq!(lines[1], "z_file.txt");
    }

    #[test]
    fn stat_output_contains_summary() {
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(&[], &summary, OutputMode::Stat, false, false, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("3 files changed"));
        assert!(output.contains("6 chunks changed"));
        assert!(output.contains("45.0 MB delta"));
    }

    #[test]
    fn stat_output_contains_large_file_reuse_metrics() {
        let reports = vec![make_chunk_metric_report("model.bin")];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(&reports, &summary, OutputMode::Stat, false, false, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("model.bin: 100 B -> 120 B"));
        assert!(output.contains("3 chunks changed"));
        assert!(output.contains("+20 B"));
        assert!(output.contains("66.7% reuse"));
        assert!(output.contains("3 files changed"));
    }

    #[test]
    fn human_output_modified_file() {
        let reports = vec![make_modified_report("model.safetensors")];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(
            &reports,
            &summary,
            OutputMode::Human,
            false,
            false,
            &mut buf,
        )
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("model.safetensors:"));
        assert!(output.contains("10.0 GB"));
        assert!(output.contains("6 chunks changed"));
        assert!(output.contains("45.0 MB delta"));
        assert!(output.contains("99.6% dedup"));
        assert!(output.contains("  tensor layers.23.attention.weight: 12.0 MB modified"));
    }

    #[test]
    fn human_output_added_file() {
        let reports = vec![make_added_report("config.json")];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(
            &reports,
            &summary,
            OutputMode::Human,
            false,
            false,
            &mut buf,
        )
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("config.json: added (1.2 KB, 1 chunks)"));
    }

    #[test]
    fn human_output_deleted_file() {
        let reports = vec![make_deleted_report("old.bin")];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(
            &reports,
            &summary,
            OutputMode::Human,
            false,
            false,
            &mut buf,
        )
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("old.bin: deleted (2.0 KB, 2 chunks)"));
    }

    #[test]
    fn human_output_sorted_by_path() {
        let reports = vec![
            make_added_report("z_last.txt"),
            make_deleted_report("a_first.bin"),
            make_modified_report("m_middle.safetensors"),
        ];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(
            &reports,
            &summary,
            OutputMode::Human,
            false,
            false,
            &mut buf,
        )
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        // First file line should be a_first, then m_middle, then z_last.
        let file_lines: Vec<&&str> = lines
            .iter()
            .filter(|l| !l.starts_with("  ") && !l.is_empty() && !l.contains("files changed"))
            .collect();
        assert!(file_lines[0].starts_with("a_first.bin"));
        assert!(file_lines[1].starts_with("m_middle.safetensors"));
        assert!(file_lines[2].starts_with("z_last.txt"));
    }

    #[test]
    fn byte_ranges_shown_when_enabled() {
        let reports = vec![make_modified_report("model.safetensors")];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(&reports, &summary, OutputMode::Human, false, true, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("bytes 4521984000\u{2013}4569169920 changed"));
    }

    #[test]
    fn verbose_shows_segment_details() {
        let mut entry = make_modified_report("model.safetensors");
        entry.report.segment_details = vec![
            SegmentDiff {
                index: 42,
                status: SegmentStatus::Removed,
                old_xorb_hash: Some("a1b2c3d4e5f6".to_string()),
                new_xorb_hash: None,
                old_chunk_range: Some((0, 3)),
                new_chunk_range: None,
                bytes: 15_728_640,
            },
            SegmentDiff {
                index: 42,
                status: SegmentStatus::Added,
                old_xorb_hash: None,
                new_xorb_hash: Some("d4e5f6a1b2c3".to_string()),
                old_chunk_range: None,
                new_chunk_range: Some((0, 3)),
                bytes: 15_728_640,
            },
        ];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(
            &[entry],
            &summary,
            OutputMode::HumanVerbose,
            false,
            false,
            &mut buf,
        )
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("segment[42]: xorb a1b2c3.. [0..3] (15.0 MB) removed"));
        assert!(output.contains("segment[42]: xorb d4e5f6.. [0..3] (15.0 MB) added"));
    }

    #[test]
    fn json_output_round_trips() {
        let reports = vec![
            make_modified_report("model.safetensors"),
            make_added_report("config.json"),
        ];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(&reports, &summary, OutputMode::Json, false, false, &mut buf).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert!(value.get("files").unwrap().is_array());
        assert!(value.get("summary").unwrap().is_object());
        let files = value["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        // Sorted by path.
        assert_eq!(files[0]["report"]["path"], "config.json");
        assert_eq!(files[1]["report"]["path"], "model.safetensors");
    }

    #[test]
    fn color_output_contains_ansi_codes() {
        let reports = vec![make_added_report("new.txt")];
        let summary = make_summary();
        let mut buf = Vec::new();
        format_diff(&reports, &summary, OutputMode::Human, true, false, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains(GREEN));
        assert!(output.contains(RESET));
    }

    #[test]
    fn git_native_file_output() {
        let entry = FileDiffEntry {
            report: ChunkDiffReport {
                path: "readme.md".to_string(),
                status: FileStatus::GitNative,
                old_size: 100,
                new_size: 200,
                unchanged_segments: 0,
                unchanged_bytes: 0,
                removed_segments: 0,
                removed_bytes: 0,
                added_segments: 0,
                added_bytes: 0,
                delta_bytes: 0,
                dedup_ratio: 0.0,
                changed_byte_ranges: vec![],
                segment_details: vec![],
                annotations: vec![],
                chunk_metrics: None,
            },
        };
        let summary = DiffSummary {
            files_changed: 1,
            total_segments_changed: 0,
            total_delta_bytes: 0,
        };
        let mut buf = Vec::new();
        format_diff(
            &[entry],
            &summary,
            OutputMode::Human,
            false,
            false,
            &mut buf,
        )
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("readme.md: git-native (chunk diff unavailable)"));
    }
}
