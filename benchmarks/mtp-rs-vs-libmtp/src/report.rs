use crate::run_info::RunInfo;
use crate::types::{
    format_bytes, format_duration, format_throughput, BackendName, BenchmarkResult, Operation,
};
use comfy_table::{presets::UTF8_FULL, Cell, ContentArrangement, Table};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Whether a result belongs in the listing-during-download table rather than the main one.
fn is_list_during(r: &BenchmarkResult) -> bool {
    matches!(r.operation, Operation::ListDuringDownload { .. })
}

/// One row of the listing-during-download table, formatted.
struct ListDuringRow {
    pacing: String,
    size: String,
    runs: String,
    download_median: String,
    throughput: String,
    list_median: String,
    list_max: String,
}

fn list_during_rows(results: &[BenchmarkResult]) -> Vec<ListDuringRow> {
    let dash = || "\u{2014}".to_string();
    results
        .iter()
        .filter(|r| is_list_during(r))
        .map(|r| {
            let Operation::ListDuringDownload { pacing } = r.operation else {
                unreachable!("filtered above")
            };
            ListDuringRow {
                pacing: pacing.to_string(),
                size: format_bytes(r.file_size_bytes),
                runs: r.durations.len().to_string(),
                download_median: format_duration(r.median_duration()),
                throughput: r
                    .throughput_bytes_per_sec()
                    .map(format_throughput)
                    .unwrap_or_else(dash),
                list_median: r
                    .median_list_latency()
                    .map(format_duration)
                    .unwrap_or_else(dash),
                list_max: r
                    .max_list_latency()
                    .map(format_duration)
                    .unwrap_or_else(dash),
            }
        })
        .collect()
}

const LIST_DURING_TITLE: &str =
    "Listing the storage root halfway through a download (mtp-rs only; medians unless noted)";

const LIST_DURING_HEADER: [&str; 7] = [
    "Pacing",
    "File Size",
    "Runs",
    "Download Median",
    "Download Throughput",
    "Listing Median",
    "Listing Max",
];

/// Group results by scenario, then by backend.
fn group_results(
    results: &[BenchmarkResult],
) -> BTreeMap<String, BTreeMap<BackendName, &BenchmarkResult>> {
    let mut map: BTreeMap<String, BTreeMap<BackendName, &BenchmarkResult>> = BTreeMap::new();
    for r in results.iter().filter(|r| !is_list_during(r)) {
        let key = scenario_key(r.operation, r.file_size_bytes);
        map.entry(key).or_default().insert(r.backend, r);
    }
    map
}

/// Build a stable, sortable string key from an operation and file size.
fn scenario_key(op: Operation, file_size: u64) -> String {
    format!("{op}|{file_size}")
}

/// Parse a scenario key back into its components.
fn parse_scenario_key(key: &str) -> (String, String) {
    let parts: Vec<&str> = key.splitn(2, '|').collect();
    let op = parts[0].to_string();
    let size: u64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let size_str = if size == 0 {
        "N/A".to_string()
    } else {
        format_bytes(size)
    };
    (op, size_str)
}

/// Formatted statistics for a single backend in a scenario row.
struct FormattedStats {
    median: String,
    stddev: String,
    throughput: String,
}

/// Extract formatted stats from an optional benchmark result.
fn format_backend_stats(result: Option<&&BenchmarkResult>) -> FormattedStats {
    let dash = || "\u{2014}".to_string(); // em-dash
    FormattedStats {
        median: result
            .map(|r| format_duration(r.median_duration()))
            .unwrap_or_else(dash),
        stddev: result
            .map(|r| format_duration(r.std_dev()))
            .unwrap_or_else(dash),
        throughput: result
            .and_then(|r| r.throughput_bytes_per_sec())
            .map(format_throughput)
            .unwrap_or_else(dash),
    }
}

/// Compute the speedup ratio (libmtp_median / mtp_rs_median).
/// Returns ">1.00x" when mtp-rs is faster, "<1.00x" when libmtp is faster.
fn compute_speedup(mtp_rs: Option<&&BenchmarkResult>, libmtp: Option<&&BenchmarkResult>) -> String {
    match (mtp_rs, libmtp) {
        (Some(m), Some(l)) => {
            let m_secs = m.median_duration().as_secs_f64();
            let l_secs = l.median_duration().as_secs_f64();
            if m_secs > 0.0 {
                format!("{:.2}x", l_secs / m_secs)
            } else {
                "N/A".to_string()
            }
        }
        _ => "N/A".to_string(),
    }
}

/// Render results as UTF-8 terminal tables for display: the run's context, then one row per
/// scenario (operation + file size) with each backend's median, std dev, and throughput, then
/// the listing-during-download table when those ran.
pub fn render_terminal_table(info: &RunInfo, results: &[BenchmarkResult]) -> String {
    let mut parts = vec![info.lines().join("\n")];
    if results.iter().any(|r| !is_list_during(r)) {
        parts.push(main_terminal_table(results));
    }
    let rows = list_during_rows(results);
    if !rows.is_empty() {
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(LIST_DURING_HEADER.map(Cell::new).to_vec());
        for r in rows {
            table.add_row(vec![
                r.pacing,
                r.size,
                r.runs,
                r.download_median,
                r.throughput,
                r.list_median,
                r.list_max,
            ]);
        }
        parts.push(format!("{LIST_DURING_TITLE}\n{table}"));
    }
    parts.join("\n\n")
}

fn main_terminal_table(results: &[BenchmarkResult]) -> String {
    let grouped = group_results(results);

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            Cell::new("Operation"),
            Cell::new("File Size"),
            Cell::new("mtp-rs Median"),
            Cell::new("mtp-rs Std Dev"),
            Cell::new("mtp-rs Throughput"),
            Cell::new("libmtp Median"),
            Cell::new("libmtp Std Dev"),
            Cell::new("libmtp Throughput"),
            Cell::new("Speedup"),
        ]);

    for (key, backends) in &grouped {
        let (op, size_str) = parse_scenario_key(key);

        let mtp_rs = backends.get(&BackendName::MtpRs);
        let libmtp = backends.get(&BackendName::Libmtp);
        let rs = format_backend_stats(mtp_rs);
        let lm = format_backend_stats(libmtp);
        let speedup = compute_speedup(mtp_rs, libmtp);

        table.add_row(vec![
            Cell::new(&op),
            Cell::new(&size_str),
            Cell::new(&rs.median),
            Cell::new(&rs.stddev),
            Cell::new(&rs.throughput),
            Cell::new(&lm.median),
            Cell::new(&lm.stddev),
            Cell::new(&lm.throughput),
            Cell::new(&speedup),
        ]);
    }

    table.to_string()
}

/// Render results as Markdown suitable for pasting into GitHub issues or READMEs: the run's
/// context as a list, the main table, and the listing-during-download table when those ran.
pub fn render_markdown_table(info: &RunInfo, results: &[BenchmarkResult]) -> String {
    let context: Vec<String> = info.lines().iter().map(|l| format!("- {l}")).collect();
    let mut parts = vec![context.join("\n")];
    if results.iter().any(|r| !is_list_during(r)) {
        parts.push(main_markdown_table(results));
    }
    let rows = list_during_rows(results);
    if !rows.is_empty() {
        let mut lines = vec![
            format!("{LIST_DURING_TITLE}:"),
            String::new(),
            format!("| {} |", LIST_DURING_HEADER.join(" | ")),
            format!(
                "|{}|",
                LIST_DURING_HEADER
                    .map(|h| "-".repeat(h.len() + 2))
                    .join("|")
            ),
        ];
        for r in rows {
            lines.push(format!(
                "| {} | {} | {} | {} | {} | {} | {} |",
                r.pacing,
                r.size,
                r.runs,
                r.download_median,
                r.throughput,
                r.list_median,
                r.list_max
            ));
        }
        parts.push(lines.join("\n"));
    }
    parts.join("\n\n")
}

fn main_markdown_table(results: &[BenchmarkResult]) -> String {
    let grouped = group_results(results);

    let mut lines = Vec::new();
    lines.push(
        "| Operation | File Size | mtp-rs Median | mtp-rs Std Dev | mtp-rs Throughput | libmtp Median | libmtp Std Dev | libmtp Throughput | Speedup |"
            .to_string(),
    );
    lines.push(
        "|-----------|-----------|---------------|----------------|-------------------|---------------|----------------|-------------------|---------|"
            .to_string(),
    );

    for (key, backends) in &grouped {
        let (op, size_str) = parse_scenario_key(key);

        let mtp_rs = backends.get(&BackendName::MtpRs);
        let libmtp = backends.get(&BackendName::Libmtp);
        let rs = format_backend_stats(mtp_rs);
        let lm = format_backend_stats(libmtp);
        let speedup = compute_speedup(mtp_rs, libmtp);

        lines.push(format!(
            "| {op} | {size_str} | {} | {} | {} | {} | {} | {} | {speedup} |",
            rs.median, rs.stddev, rs.throughput, lm.median, lm.stddev, lm.throughput,
        ));
    }

    lines.join("\n")
}

/// Render results as CSV text, preceded by the run's context as `# key: value` comment lines
/// (skip lines starting with `#` when loading it).
///
/// Columns: operation, file_size_bytes, file_size_human, backend, runs, median_secs,
/// stddev_secs, throughput_bytes_per_sec, list_median_secs, list_max_secs (the last two empty
/// except for the listing-during-download rows)
pub fn render_csv(info: &RunInfo, results: &[BenchmarkResult]) -> String {
    let mut lines: Vec<String> = info.lines().iter().map(|l| format!("# {l}")).collect();
    lines.push(
        "operation,file_size_bytes,file_size_human,backend,runs,median_secs,stddev_secs,throughput_bytes_per_sec,list_median_secs,list_max_secs"
            .to_string(),
    );

    let secs = |d: Option<std::time::Duration>| {
        d.map(|d| format!("{:.6}", d.as_secs_f64()))
            .unwrap_or_default()
    };
    for r in results {
        let tp = r
            .throughput_bytes_per_sec()
            .map(|v| format!("{:.2}", v))
            .unwrap_or_default();

        lines.push(format!(
            "{},{},{},{},{},{:.6},{:.6},{},{},{}",
            r.operation,
            r.file_size_bytes,
            format_bytes(r.file_size_bytes),
            r.backend,
            r.durations.len(),
            r.median_duration().as_secs_f64(),
            r.std_dev().as_secs_f64(),
            tp,
            secs(r.median_list_latency()),
            secs(r.max_list_latency()),
        ));
    }

    lines.join("\n")
}

/// The JSON report: the run's context next to the raw per-run results.
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonReport {
    /// Version, device, link speed, date, and OS.
    pub run: RunInfo,
    /// Every scenario, with each measured run's duration.
    pub results: Vec<BenchmarkResult>,
}

/// Render results as a JSON string: `{ "run": {...}, "results": [...] }`.
pub fn render_json(info: &RunInfo, results: &[BenchmarkResult]) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&JsonReport {
        run: info.clone(),
        results: results.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Operation, Pacing};
    use std::time::Duration;

    fn sample_results() -> Vec<BenchmarkResult> {
        vec![
            BenchmarkResult::new(
                BackendName::MtpRs,
                Operation::Download,
                10_000_000,
                vec![
                    Duration::from_millis(500),
                    Duration::from_millis(520),
                    Duration::from_millis(480),
                ],
            ),
            BenchmarkResult::new(
                BackendName::Libmtp,
                Operation::Download,
                10_000_000,
                vec![
                    Duration::from_millis(800),
                    Duration::from_millis(850),
                    Duration::from_millis(780),
                ],
            ),
            BenchmarkResult::new(
                BackendName::MtpRs,
                Operation::ListFiles,
                0,
                vec![
                    Duration::from_millis(50),
                    Duration::from_millis(55),
                    Duration::from_millis(48),
                ],
            ),
        ]
    }

    fn with_list_during() -> Vec<BenchmarkResult> {
        let mut results = sample_results();
        for (pacing, download_ms, list_ms) in [
            (Pacing::HeldOpen, 15_000, [7_400, 7_500, 7_600]),
            (Pacing::Default, 15_200, [900, 1_200, 1_800]),
        ] {
            let mut r = BenchmarkResult::new(
                BackendName::MtpRs,
                Operation::ListDuringDownload { pacing },
                4_000_000_000,
                vec![Duration::from_millis(download_ms); 3],
            );
            r.list_latencies = list_ms.map(Duration::from_millis).to_vec();
            results.push(r);
        }
        results
    }

    fn info() -> RunInfo {
        RunInfo {
            date: "2026-11-11 10:00 +01:00".into(),
            mtp_rs_version: "0.38.0".into(),
            usb_speed: "USB 3.2 Gen 1 (5 Gbit/s)".into(),
            ..RunInfo::default()
        }
    }

    #[test]
    fn test_csv_output() {
        let csv = render_csv(&info(), &sample_results());
        let lines: Vec<&str> = csv.lines().filter(|l| !l.starts_with('#')).collect();
        // Header + 3 data rows
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("operation,"));
        assert!(lines[1].contains("mtp-rs"));
        assert!(lines[2].contains("libmtp"));
        assert!(csv.contains("# USB link:   USB 3.2 Gen 1"));
        // Every row has as many fields as the header.
        let width = lines[0].split(',').count();
        assert!(lines.iter().all(|l| l.split(',').count() == width));
    }

    #[test]
    fn csv_carries_listing_latencies_for_list_during_rows() {
        let csv = render_csv(&info(), &with_list_during());
        let row = csv
            .lines()
            .find(|l| l.starts_with("list_during_paced,"))
            .unwrap();
        assert!(row.ends_with(",1.200000,1.800000"), "{row}");
        let plain = csv.lines().find(|l| l.starts_with("download,")).unwrap();
        assert!(plain.ends_with(",,"), "{plain}");
    }

    #[test]
    fn test_markdown_output() {
        let md = render_markdown_table(&info(), &sample_results());
        assert!(md.contains("| Operation |"));
        assert!(md.contains("|--------"));
        assert!(md.contains("download"));
        assert!(md.contains("- mtp-rs:     0.38.0"));
        assert!(md.contains("- Date:       2026-11-11"));
        assert!(!md.contains("halfway"));
    }

    #[test]
    fn list_during_gets_its_own_table_with_cost_and_benefit_side_by_side() {
        let md = render_markdown_table(&info(), &with_list_during());
        let (main, list_during) = md.split_once("halfway").unwrap();
        assert!(!main.contains("list_during"), "{main}");
        let held = list_during
            .lines()
            .find(|l| l.starts_with("| held open |"))
            .unwrap();
        assert_eq!(
            held,
            "| held open | 4.0 GB | 3 | 15.000s | 266.67 MB/s | 7.500s | 7.600s |"
        );
        assert!(list_during.contains("| default (~2 s windows) | 4.0 GB | 3 | 15.200s |"));
        // The header and separator have the same column count as the rows.
        let cols = |l: &str| l.matches('|').count();
        let table: Vec<&str> = list_during.lines().filter(|l| l.starts_with('|')).collect();
        assert!(table.iter().all(|l| cols(l) == cols(table[0])), "{table:?}");

        let only = render_markdown_table(&info(), &with_list_during()[3..]);
        assert!(!only.contains("| Operation |"), "{only}");
    }

    #[test]
    fn test_terminal_table_output() {
        let table = render_terminal_table(&info(), &with_list_during());
        // Should contain header text
        assert!(table.contains("Operation"));
        assert!(table.contains("mtp-rs Median"));
        assert!(table.contains("Listing Median"));
        assert!(table.contains("USB 3.2 Gen 1"));
    }

    #[test]
    fn test_json_output() {
        let json = render_json(&info(), &with_list_during()).unwrap();
        let parsed: JsonReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.results.len(), 5);
        assert_eq!(parsed.run.mtp_rs_version, "0.38.0");
        assert_eq!(parsed.results[4].list_latencies.len(), 3);
    }
}
