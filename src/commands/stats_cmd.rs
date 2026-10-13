//! `lox stats` — read the statistics the Miniserver logs to its SD card.
//!
//! The Miniserver serves an HTML index at `/stats/` with one entry per
//! statistic and month, and each month as a self-describing XML file
//! (`/stats/<uuid>[_<group>].<YYYYMM>.xml`). Everything here is read-only.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, Event};
use serde::Serialize;
use std::collections::HashSet;
use std::io::Write;

use crate::client::LoxClient;
use crate::commands::RunContext;
use crate::config::Config;

#[derive(Subcommand)]
pub enum StatsCmd {
    /// List the statistics the Miniserver keeps: GET /stats/
    List {
        /// Case-insensitive substring match on the label
        #[arg(long)]
        filter: Option<String>,
        /// Only this month (YYYY-MM)
        #[arg(long, value_parser = parse_month)]
        month: Option<String>,
        /// One row per statistic and group, newest month only
        #[arg(long)]
        latest: bool,
    },
    /// Print the logged values of one statistic across months
    Get {
        /// Statistic name (case-insensitive, exact match first, then substring) or UUID
        name: String,
        /// Statistic group, for grouped statistics (e.g. meters)
        #[arg(long)]
        group: Option<u32>,
        /// First month to include (YYYY-MM, inclusive; default: oldest available)
        #[arg(long, value_parser = parse_month)]
        from: Option<String>,
        /// Last month to include (YYYY-MM, inclusive; default: newest available)
        #[arg(long, value_parser = parse_month)]
        to: Option<String>,
    },
}

pub fn cmd_stats(ctx: &RunContext, action: StatsCmd) -> Result<()> {
    let cfg = Config::load()?;
    let lox = LoxClient::new(cfg)?;
    let mut out = std::io::stdout().lock();
    match action {
        StatsCmd::List {
            filter,
            month,
            latest,
        } => list(
            ctx,
            &lox,
            filter.as_deref(),
            month.as_deref(),
            latest,
            &mut out,
        ),
        StatsCmd::Get {
            name,
            group,
            from,
            to,
        } => get(ctx, &lox, &name, group, &MonthRange { from, to }, &mut out),
    }
}

/// Validate a `YYYY-MM` argument.
fn parse_month(s: &str) -> std::result::Result<String, String> {
    chrono::NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d")
        .ok()
        .filter(|_| s.len() == 7)
        .map(|_| s.to_string())
        .ok_or_else(|| format!("expected YYYY-MM, got '{s}'"))
}

/// Inclusive month range; `None` bounds are open.
struct MonthRange {
    from: Option<String>,
    to: Option<String>,
}

impl MonthRange {
    fn contains(&self, month: &str) -> bool {
        self.from.as_deref().is_none_or(|f| month >= f)
            && self.to.as_deref().is_none_or(|t| month <= t)
    }
}

/// One file listed in the `/stats/` index.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatEntry {
    /// File name as served, e.g. `1e50..._2.202610.xml`
    pub file: String,
    pub uuid: String,
    pub group: Option<u32>,
    /// Month as `YYYY-MM`
    pub month: String,
    /// Statistic name (entity-decoded, group and room parts stripped)
    pub name: String,
    /// Full entity-decoded label without the trailing `YYYYMM`
    pub label: String,
}

/// The logged values of one statistic file (or several merged months).
#[derive(Debug, Clone, PartialEq)]
pub struct StatSeries {
    pub name: String,
    pub group: Option<u32>,
    /// Output labels, from `Outputs="a,b"`
    pub outputs: Vec<String>,
    /// `(timestamp, values in output order)`
    pub rows: Vec<(String, Vec<f64>)>,
}

/// Quote a CSV field when it contains a separator, quote or newline.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn group_label(group: Option<u32>) -> String {
    group.map_or_else(|| "-".to_string(), |g| g.to_string())
}

/// Fetch and parse the `/stats/` index.
fn fetch_index(lox: &LoxClient) -> Result<Vec<StatEntry>> {
    let html = lox
        .get_text("/stats/")
        .context("reading the statistics index (/stats/)")?;
    Ok(parse_index(&html))
}

fn fetch_series(lox: &LoxClient, entry: &StatEntry) -> Result<StatSeries> {
    let xml = lox
        .get_text(&format!("/stats/{}", entry.file))
        .with_context(|| format!("reading {}", entry.file))?;
    parse_series(&xml).with_context(|| format!("parsing {}", entry.file))
}

fn list(
    ctx: &RunContext,
    lox: &LoxClient,
    filter: Option<&str>,
    month: Option<&str>,
    latest: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let filter = filter.map(|f| f.to_lowercase());
    let mut entries: Vec<StatEntry> = fetch_index(lox)?
        .into_iter()
        .filter(|e| {
            filter
                .as_deref()
                .is_none_or(|f| e.label.to_lowercase().contains(f))
        })
        .filter(|e| month.is_none_or(|m| e.month == m))
        .collect();
    if latest {
        let mut newest: std::collections::HashMap<(String, Option<u32>), String> =
            std::collections::HashMap::new();
        for e in &entries {
            let m = newest.entry((e.uuid.clone(), e.group)).or_default();
            if e.month > *m {
                *m = e.month.clone();
            }
        }
        entries.retain(|e| newest[&(e.uuid.clone(), e.group)] == e.month);
    }

    if ctx.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&entries)?)?;
    } else if ctx.csv {
        if !ctx.no_header {
            writeln!(out, "NAME,GROUP,MONTH,UUID,LABEL")?;
        }
        for e in &entries {
            writeln!(
                out,
                "{},{},{},{},{}",
                csv_field(&e.name),
                e.group.map(|g| g.to_string()).unwrap_or_default(),
                e.month,
                e.uuid,
                csv_field(&e.label)
            )?;
        }
    } else {
        let w = entries
            .iter()
            .map(|e| e.name.chars().count())
            .max()
            .unwrap_or(0)
            .max(4);
        if !ctx.no_header {
            writeln!(
                out,
                "{:<w$}  {:<5}  {:<7}  {:<35}  LABEL",
                "NAME", "GROUP", "MONTH", "UUID"
            )?;
        }
        for e in &entries {
            writeln!(
                out,
                "{:<w$}  {:<5}  {:<7}  {:<35}  {}",
                e.name,
                group_label(e.group),
                e.month,
                e.uuid,
                e.label
            )?;
        }
    }
    Ok(())
}

/// Resolve a name or uuid to the index entries of exactly one statistic uuid.
fn resolve<'a>(entries: &'a [StatEntry], query: &str) -> Result<Vec<&'a StatEntry>> {
    let q = query.to_lowercase();
    let mut matches: Vec<&StatEntry> = entries.iter().filter(|e| e.uuid == q).collect();
    if matches.is_empty() {
        matches = entries
            .iter()
            .filter(|e| e.name.to_lowercase() == q)
            .collect();
    }
    if matches.is_empty() {
        matches = entries
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&q))
            .collect();
    }
    if matches.is_empty() {
        let mut names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        names.dedup();
        return Err(crate::errors::not_found_error(
            "Statistic",
            query,
            &names,
            "lox stats list",
        ));
    }
    let uuids: HashSet<&str> = matches.iter().map(|e| e.uuid.as_str()).collect();
    if uuids.len() > 1 {
        let mut candidates: Vec<(&str, Option<u32>, &str)> = matches
            .iter()
            .map(|e| (e.name.as_str(), e.group, e.uuid.as_str()))
            .collect();
        candidates.sort();
        candidates.dedup();
        let lines: Vec<String> = candidates
            .iter()
            .map(|(name, group, uuid)| format!("  {name}  group {}  {uuid}", group_label(*group)))
            .collect();
        bail!(
            "Ambiguous: '{query}' matches {} statistics; pass the uuid instead:\n{}",
            uuids.len(),
            lines.join("\n")
        );
    }
    Ok(matches)
}

fn get(
    ctx: &RunContext,
    lox: &LoxClient,
    query: &str,
    group: Option<u32>,
    range: &MonthRange,
    out: &mut dyn Write,
) -> Result<()> {
    let index = fetch_index(lox)?;
    let matches = resolve(&index, query)?;
    let name = matches[0].name.clone();
    let uuid = matches[0].uuid.clone();

    let mut groups: Vec<Option<u32>> = matches.iter().map(|e| e.group).collect();
    groups.sort();
    groups.dedup();
    let group = match group {
        Some(g) => {
            if !groups.contains(&Some(g)) {
                let available: Vec<String> = groups.iter().map(|g| group_label(*g)).collect();
                bail!(
                    "Statistic '{name}' ({uuid}) has no group {g}. Available groups: {}",
                    available.join(", ")
                );
            }
            Some(g)
        }
        None if groups.len() > 1 => {
            let mut lines = Vec::new();
            for g in &groups {
                let newest = matches
                    .iter()
                    .filter(|e| e.group == *g)
                    .max_by(|a, b| a.month.cmp(&b.month))
                    .expect("group taken from matches");
                let series = fetch_series(lox, newest)?;
                lines.push(format!(
                    "  group {}: {}",
                    group_label(*g),
                    series.outputs.join(", ")
                ));
            }
            bail!(
                "Statistic '{name}' ({uuid}) has {} groups; pass --group <N>:\n{}",
                groups.len(),
                lines.join("\n")
            );
        }
        None => groups[0],
    };

    let mut selected: Vec<&StatEntry> = matches.into_iter().filter(|e| e.group == group).collect();
    selected.sort_by(|a, b| a.month.cmp(&b.month));
    let first = selected
        .first()
        .map(|e| e.month.clone())
        .unwrap_or_default();
    let last = selected.last().map(|e| e.month.clone()).unwrap_or_default();
    selected.retain(|e| range.contains(&e.month));
    if selected.is_empty() {
        bail!("No months of '{name}' in the requested range (available: {first} to {last})");
    }

    let parts = selected
        .iter()
        .map(|e| fetch_series(lox, e))
        .collect::<Result<Vec<_>>>()?;
    let series = merge_months(parts);
    write_series(ctx, &series, out)
}

fn write_series(ctx: &RunContext, series: &StatSeries, out: &mut dyn Write) -> Result<()> {
    if ctx.json {
        let rows: Vec<serde_json::Value> = series
            .rows
            .iter()
            .map(|(t, values)| {
                let values: serde_json::Map<String, serde_json::Value> = series
                    .outputs
                    .iter()
                    .cloned()
                    .zip(values.iter().map(|v| serde_json::json!(v)))
                    .collect();
                serde_json::json!({ "t": t, "values": values })
            })
            .collect();
        let doc = serde_json::json!({
            "name": series.name,
            "group": series.group,
            "outputs": series.outputs,
            "rows": rows,
        });
        writeln!(out, "{}", serde_json::to_string_pretty(&doc)?)?;
    } else if ctx.csv {
        if !ctx.no_header {
            let header: Vec<String> = std::iter::once("T".to_string())
                .chain(series.outputs.iter().map(|o| csv_field(o)))
                .collect();
            writeln!(out, "{}", header.join(","))?;
        }
        for (t, values) in &series.rows {
            let values: Vec<String> = values.iter().map(|v| v.to_string()).collect();
            writeln!(out, "{t},{}", values.join(","))?;
        }
    } else {
        let widths: Vec<usize> = series
            .outputs
            .iter()
            .enumerate()
            .map(|(i, o)| {
                series
                    .rows
                    .iter()
                    .map(|(_, v)| v[i].to_string().len())
                    .max()
                    .unwrap_or(0)
                    .max(o.chars().count())
            })
            .collect();
        if !ctx.no_header {
            let mut line = format!("{:<19}", "T");
            for (o, w) in series.outputs.iter().zip(&widths) {
                line += &format!("  {o:>w$}");
            }
            writeln!(out, "{}", line.trim_end())?;
        }
        for (t, values) in &series.rows {
            let mut line = format!("{t:<19}");
            for (v, w) in values.iter().zip(&widths) {
                line += &format!("  {:>w$}", v.to_string());
            }
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// Split a stats file name into `(uuid, group, "YYYY-MM")`.
///
/// Accepts `<uuid>.<YYYYMM>.xml` and `<uuid>_<group>.<YYYYMM>.xml`, where the
/// uuid is the Loxone form `8-4-4-16` lowercase hex. Parsed by hand so no
/// regex dependency is needed.
fn parse_file_name(file: &str) -> Option<(String, Option<u32>, String)> {
    let stem = file.strip_suffix(".xml")?;
    let (stem, month) = stem.rsplit_once('.')?;
    if month.len() != 6 || !month.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (uuid, group) = match stem.split_once('_') {
        Some((uuid, group)) => {
            if group.is_empty() || !group.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (uuid, Some(group.parse::<u32>().ok()?))
        }
        None => (stem, None),
    };
    let parts: Vec<&str> = uuid.split('-').collect();
    let lens: Vec<usize> = parts.iter().map(|p| p.len()).collect();
    if lens != [8, 4, 4, 16]
        || !parts
            .iter()
            .all(|p| p.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    {
        return None;
    }
    Some((
        uuid.to_string(),
        group,
        format!("{}-{}", &month[..4], &month[4..]),
    ))
}

/// Parse the HTML index served at `/stats/`.
///
/// Only `<li><a href="...">label</a></li>` entries whose href is a stats file
/// name are returned; anything else (stylesheet, foreign files) is skipped.
pub fn parse_index(html: &str) -> Vec<StatEntry> {
    let mut entries = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("<li>") {
        rest = &rest[pos + 4..];
        let Some(after) = rest.trim_start().strip_prefix("<a href=\"") else {
            continue;
        };
        let Some((href, after)) = after.split_once('"') else {
            break;
        };
        let Some(gt) = after.find('>') else {
            break;
        };
        let after = &after[gt + 1..];
        let Some((raw_label, _)) = after.split_once("</a>") else {
            break;
        };
        let Some((uuid, group, month)) = parse_file_name(href) else {
            continue;
        };
        let decoded = unescape(raw_label)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| raw_label.to_string());
        let yyyymm = month.replace('-', "");
        let label = decoded
            .trim()
            .strip_suffix(yyyymm.as_str())
            .unwrap_or(decoded.trim())
            .trim_end()
            .to_string();
        let name = label
            .split_once(" (")
            .map_or(label.as_str(), |(n, _)| n)
            .trim()
            .to_string();
        entries.push(StatEntry {
            file: href.to_string(),
            uuid,
            group,
            month,
            name,
            label,
        });
    }
    entries
}

/// Read an attribute as an entity-decoded string.
fn attr(e: &BytesStart, name: &[u8]) -> Result<Option<String>> {
    for a in e.attributes() {
        let a = a.context("malformed XML attribute")?;
        if a.key.as_ref() == name {
            return Ok(Some(a.unescape_value()?.into_owned()));
        }
    }
    Ok(None)
}

/// Parse one statistics XML file (`<Statistics ...><S T=".." V=".." V2=".."/>`).
pub fn parse_series(xml: &str) -> Result<StatSeries> {
    let mut reader = Reader::from_str(xml);
    let mut series: Option<StatSeries> = None;
    // Attribute names holding the values, in output order: V, V2, V3, ...
    let mut value_attrs: Vec<String> = Vec::new();
    loop {
        match reader.read_event().context("parsing statistics XML")? {
            Event::Start(e) | Event::Empty(e) => match e.name().as_ref() {
                b"Statistics" => {
                    let name = attr(&e, b"Name")?.unwrap_or_default();
                    let group = match attr(&e, b"StatsGroup")? {
                        Some(g) => Some(
                            g.trim()
                                .parse::<u32>()
                                .with_context(|| format!("invalid StatsGroup '{g}'"))?,
                        ),
                        None => None,
                    };
                    let mut outputs: Vec<String> = attr(&e, b"Outputs")?
                        .map(|o| o.split(',').map(|s| s.trim().to_string()).collect())
                        .unwrap_or_default();
                    let num = match attr(&e, b"NumOutputs")? {
                        Some(n) => n
                            .trim()
                            .parse::<usize>()
                            .with_context(|| format!("invalid NumOutputs '{n}'"))?,
                        None => outputs.len().max(1),
                    };
                    value_attrs = (1..=num)
                        .map(|i| if i == 1 { "V".into() } else { format!("V{i}") })
                        .collect();
                    // Fall back to the attribute name for unlabelled outputs.
                    outputs.resize(num, String::new());
                    for (o, a) in outputs.iter_mut().zip(&value_attrs) {
                        if o.is_empty() {
                            *o = a.clone();
                        }
                    }
                    series = Some(StatSeries {
                        name,
                        group,
                        outputs,
                        rows: Vec::new(),
                    });
                }
                b"S" => {
                    let Some(s) = series.as_mut() else {
                        bail!("statistics row before <Statistics> element");
                    };
                    let t = attr(&e, b"T")?.context("statistics row without T attribute")?;
                    let mut values = Vec::with_capacity(value_attrs.len());
                    for a in &value_attrs {
                        let raw = attr(&e, a.as_bytes())?
                            .with_context(|| format!("row {t}: missing value {a}"))?;
                        let v = raw
                            .trim()
                            .parse::<f64>()
                            .with_context(|| format!("row {t}: {a}='{raw}' is not a number"))?;
                        values.push(v);
                    }
                    s.rows.push((t, values));
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    series.context("no <Statistics> element in statistics file")
}

/// Concatenate month files (in month order) into one series.
///
/// A month file can end with the first timestamp of the next month; rows whose
/// timestamp was already emitted by an earlier month are dropped (P5).
pub fn merge_months(parts: Vec<StatSeries>) -> StatSeries {
    let mut parts = parts.into_iter();
    let Some(mut merged) = parts.next() else {
        return StatSeries {
            name: String::new(),
            group: None,
            outputs: Vec::new(),
            rows: Vec::new(),
        };
    };
    let mut seen: HashSet<String> = merged.rows.iter().map(|(t, _)| t.clone()).collect();
    for part in parts {
        let fresh: Vec<_> = part
            .rows
            .into_iter()
            .filter(|(t, _)| !seen.contains(t))
            .collect();
        seen.extend(fresh.iter().map(|(t, _)| t.clone()));
        merged.rows.extend(fresh);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_A: &str = "1a2b3c4d-0001-0002-ffff0123456789ab";
    const UUID_B: &str = "1a2b3c4d-0003-0004-ffffabcdef012345";

    const INDEX: &str = r#"<html><head><title>Statistics</title><link href="styles.css" rel="stylesheet"></head><body><ul>
<li><a href="1a2b3c4d-0001-0002-ffff0123456789ab_2.202609.xml">Heat Pump (Statistic Group: 2) (Energy, Utility Room) 202609</a></li>
<li><a href="1a2b3c4d-0003-0004-ffffabcdef012345.202610.xml">Kitchen (Climate, Kitchen) 202610</a></li>
<li><a href="1a2b3c4d-0003-0004-ffffabcdef012345.202610.xml.bak">Kitchen backup 202610</a></li>
<li><a href="not-a-uuid.202610.xml">Broken (Climate) 202610</a></li>
<li><a href="1a2b3c4d-0003-0004-ffffabcdef012345.2026.xml">Short month 2026</a></li>
<li><a href="1a2b3c4d-0001-0002-ffff0123456789ab_x.202609.xml">Bad group 202609</a></li>
<li><a href="styles.css">styles.css</a></li>
<li><a href="1a2b3c4d-0001-0002-ffff0123456789ab_1.202610.xml">Fotovolta&#xEF;ek &amp; Co (Statistic Group: 1) (Energy, Roof) 202610</a></li>
</ul></body></html>"#;

    // ── parse_index ──────────────────────────────────────────────────────────

    #[test]
    fn index_parses_grouped_ungrouped_and_encoded_labels() {
        let entries = parse_index(INDEX);
        assert_eq!(entries.len(), 3, "{entries:#?}");

        assert_eq!(
            entries[0],
            StatEntry {
                file: format!("{UUID_A}_2.202609.xml"),
                uuid: UUID_A.into(),
                group: Some(2),
                month: "2026-09".into(),
                name: "Heat Pump".into(),
                label: "Heat Pump (Statistic Group: 2) (Energy, Utility Room)".into(),
            }
        );
        assert_eq!(
            entries[1],
            StatEntry {
                file: format!("{UUID_B}.202610.xml"),
                uuid: UUID_B.into(),
                group: None,
                month: "2026-10".into(),
                name: "Kitchen".into(),
                label: "Kitchen (Climate, Kitchen)".into(),
            }
        );
        assert_eq!(entries[2].name, "Fotovoltaïek & Co");
        assert_eq!(
            entries[2].label,
            "Fotovoltaïek & Co (Statistic Group: 1) (Energy, Roof)"
        );
        assert_eq!(entries[2].group, Some(1));
    }

    #[test]
    fn index_skips_stylesheet_and_malformed_hrefs() {
        let entries = parse_index(INDEX);
        assert!(!entries.is_empty());
        assert!(
            entries
                .iter()
                .all(|e| e.file.ends_with(".xml") && !e.file.contains("_x"))
        );
        assert!(!entries.iter().any(|e| e.file == "styles.css"));
        assert!(!entries.iter().any(|e| e.name == "Broken"));
    }

    // ── parse_series ─────────────────────────────────────────────────────────

    #[test]
    fn series_with_one_output() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Statistics Name="Kitchen" NumOutputs="1" Outputs="Temperature">
<S T="2026-10-01 00:00:00" V="21.5"/>
<S T="2026-10-01 00:30:00" V="21.25"/>
</Statistics>"#;
        let s = parse_series(xml).unwrap();
        assert_eq!(s.name, "Kitchen");
        assert_eq!(s.group, None);
        assert_eq!(s.outputs, vec!["Temperature"]);
        assert_eq!(
            s.rows,
            vec![
                ("2026-10-01 00:00:00".to_string(), vec![21.5]),
                ("2026-10-01 00:30:00".to_string(), vec![21.25]),
            ]
        );
    }

    #[test]
    fn series_with_two_outputs_in_order() {
        let xml = r#"<Statistics Name="Heat Pump &amp; Boiler" StatsGroup="2" NumOutputs="2" Outputs="Consumed,Delivered">
<S T="2026-10-01 02:00:00" V="100.5" V2="42"/>
<S T="2026-10-01 03:00:00" V2="43.5" V="101"/>
</Statistics>"#;
        let s = parse_series(xml).unwrap();
        assert_eq!(s.name, "Heat Pump & Boiler");
        assert_eq!(s.group, Some(2));
        assert_eq!(s.outputs, vec!["Consumed", "Delivered"]);
        assert_eq!(s.rows[0].1, vec![100.5, 42.0]);
        assert_eq!(s.rows[1].1, vec![101.0, 43.5]);
    }

    #[test]
    fn series_row_missing_output_names_timestamp() {
        let xml = r#"<Statistics Name="Heat Pump" StatsGroup="1" NumOutputs="2" Outputs="Consumed,Delivered">
<S T="2026-10-01 02:00:00" V="1" V2="2"/>
<S T="2026-10-01 03:00:00" V="3"/>
</Statistics>"#;
        let err = parse_series(xml).unwrap_err().to_string();
        assert!(err.contains("2026-10-01 03:00:00"), "{err}");
        assert!(err.contains("V2"), "{err}");
    }

    #[test]
    fn series_non_numeric_value_is_error() {
        let xml = r#"<Statistics Name="Kitchen" NumOutputs="1" Outputs="Temperature">
<S T="2026-10-01 00:00:00" V="warm"/>
</Statistics>"#;
        let err = parse_series(xml).unwrap_err().to_string();
        assert!(err.contains("warm"), "{err}");
        assert!(err.contains("2026-10-01 00:00:00"), "{err}");
    }

    // ── merge_months ─────────────────────────────────────────────────────────

    fn series(rows: &[(&str, f64)]) -> StatSeries {
        StatSeries {
            name: "Heat Pump".into(),
            group: Some(1),
            outputs: vec!["Consumed".into()],
            rows: rows
                .iter()
                .map(|(t, v)| (t.to_string(), vec![*v]))
                .collect(),
        }
    }

    #[test]
    fn merge_drops_boundary_duplicate_and_keeps_month_order() {
        let sep = series(&[("2026-09-30 23:00:00", 1.0), ("2026-10-01 02:00:00", 2.0)]);
        let oct = series(&[("2026-10-01 02:00:00", 2.0), ("2026-10-01 03:00:00", 3.0)]);
        let m = merge_months(vec![sep, oct]);
        assert_eq!(m.name, "Heat Pump");
        assert_eq!(m.group, Some(1));
        assert_eq!(m.outputs, vec!["Consumed"]);
        let ts: Vec<&str> = m.rows.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            ts,
            vec![
                "2026-09-30 23:00:00",
                "2026-10-01 02:00:00",
                "2026-10-01 03:00:00"
            ]
        );
    }

    // ── list / get (httpmock) ────────────────────────────────────────────────

    use crate::client::HttpStatusError;
    use httpmock::prelude::*;

    const HP: &str = "0a0b0c0d-0001-0002-aaaa000000000001";
    const KITCHEN: &str = "0a0b0c0d-0003-0004-bbbb000000000002";
    const OTHER_1: &str = "0a0b0c0d-0005-0006-cccc000000000003";
    const OTHER_2: &str = "0a0b0c0d-0007-0008-dddd000000000004";

    fn li(file: &str, label: &str) -> String {
        format!("<li><a href=\"{file}\">{label}</a></li>\n")
    }

    /// Index: Heat Pump groups 1 (2026-10) and 2 (2026-08..2026-11),
    /// Kitchen ungrouped (2026-09, 2026-10), two different "Other" statistics.
    fn mock_index() -> String {
        let mut html = String::from(
            "<html><head><link href=\"styles.css\" rel=\"stylesheet\"></head><body><ul>\n",
        );
        html += &li(
            &format!("{HP}_1.202610.xml"),
            "Heat Pump (Statistic Group: 1) (Energy, Utility Room) 202610",
        );
        for m in ["202608", "202609", "202610", "202611"] {
            html += &li(
                &format!("{HP}_2.{m}.xml"),
                &format!("Heat Pump (Statistic Group: 2) (Energy, Utility Room) {m}"),
            );
        }
        for m in ["202609", "202610"] {
            html += &li(
                &format!("{KITCHEN}.{m}.xml"),
                &format!("Kitchen (Climate, Kitchen) {m}"),
            );
        }
        html += &li(
            &format!("{OTHER_1}.202610.xml"),
            "Other (Climate, Attic) 202610",
        );
        html += &li(
            &format!("{OTHER_2}.202610.xml"),
            "Other (Energy, Garage) 202610",
        );
        html + "</ul></body></html>"
    }

    fn mock_client(server: &MockServer) -> LoxClient {
        LoxClient::new(Config {
            host: server.base_url(),
            user: "test".into(),
            pass: "test".into(),
            ..Default::default()
        })
        .unwrap()
    }

    fn run_ctx(json: bool, csv: bool) -> RunContext {
        RunContext {
            json,
            quiet: false,
            csv,
            dry_run: false,
            no_header: false,
            non_interactive: true,
            trace_id: None,
        }
    }

    fn serve_index(server: &MockServer) -> httpmock::Mock<'_> {
        let body = mock_index();
        server.mock(move |when, then| {
            when.method(GET).path("/stats/");
            then.status(200).body(body);
        })
    }

    fn serve_file<'a>(server: &'a MockServer, file: &str, body: &str) -> httpmock::Mock<'a> {
        let path = format!("/stats/{file}");
        let body = body.to_string();
        server.mock(move |when, then| {
            when.method(GET).path(path);
            then.status(200).body(body);
        })
    }

    fn heat_pump_xml(group: u32, t: &str) -> String {
        format!(
            "<Statistics Name=\"Heat Pump\" StatsGroup=\"{group}\" NumOutputs=\"2\" \
             Outputs=\"Consumed,Delivered\">\n<S T=\"{t}\" V=\"1\" V2=\"2\"/>\n</Statistics>"
        )
    }

    fn run_get(
        server: &MockServer,
        ctx: &RunContext,
        query: &str,
        group: Option<u32>,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<String> {
        let lox = mock_client(server);
        let mut out = Vec::new();
        let range = MonthRange {
            from: from.map(String::from),
            to: to.map(String::from),
        };
        get(ctx, &lox, query, group, &range, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn get_merges_two_months_as_csv_with_output_header() {
        let server = MockServer::start();
        serve_index(&server);
        serve_file(
            &server,
            &format!("{KITCHEN}.202609.xml"),
            r#"<Statistics Name="Kitchen" NumOutputs="2" Outputs="Temperature,Humidity">
<S T="2026-09-30 23:30:00" V="21.5" V2="48"/>
<S T="2026-10-01 00:00:00" V="21.25" V2="49"/>
</Statistics>"#,
        );
        serve_file(
            &server,
            &format!("{KITCHEN}.202610.xml"),
            r#"<Statistics Name="Kitchen" NumOutputs="2" Outputs="Temperature,Humidity">
<S T="2026-10-01 00:00:00" V="21.25" V2="49"/>
<S T="2026-10-01 00:30:00" V="21" V2="50.5"/>
</Statistics>"#,
        );
        let out = run_get(&server, &run_ctx(false, true), "kitchen", None, None, None).unwrap();
        assert_eq!(
            out,
            "T,Temperature,Humidity\n\
             2026-09-30 23:30:00,21.5,48\n\
             2026-10-01 00:00:00,21.25,49\n\
             2026-10-01 00:30:00,21,50.5\n"
        );
    }

    #[test]
    fn get_fetches_only_months_in_range() {
        let server = MockServer::start();
        serve_index(&server);
        let aug = serve_file(
            &server,
            &format!("{HP}_2.202608.xml"),
            &heat_pump_xml(2, "2026-08-01 02:00:00"),
        );
        let sep = serve_file(
            &server,
            &format!("{HP}_2.202609.xml"),
            &heat_pump_xml(2, "2026-09-01 02:00:00"),
        );
        let oct = serve_file(
            &server,
            &format!("{HP}_2.202610.xml"),
            &heat_pump_xml(2, "2026-10-01 02:00:00"),
        );
        let nov = serve_file(
            &server,
            &format!("{HP}_2.202611.xml"),
            &heat_pump_xml(2, "2026-11-01 02:00:00"),
        );
        let out = run_get(
            &server,
            &run_ctx(true, false),
            "Heat Pump",
            Some(2),
            Some("2026-09"),
            Some("2026-10"),
        )
        .unwrap();
        aug.assert_hits(0);
        sep.assert_hits(1);
        oct.assert_hits(1);
        nov.assert_hits(0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["name"], "Heat Pump");
        assert_eq!(v["group"], 2);
        assert_eq!(v["outputs"], serde_json::json!(["Consumed", "Delivered"]));
        assert_eq!(v["rows"].as_array().unwrap().len(), 2);
        assert_eq!(v["rows"][0]["t"], "2026-09-01 02:00:00");
        assert_eq!(v["rows"][0]["values"]["Delivered"], 2.0);
    }

    #[test]
    fn get_ambiguous_name_lists_both_uuids() {
        let server = MockServer::start();
        serve_index(&server);
        let err = run_get(&server, &run_ctx(false, false), "other", None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Ambiguous: 'other'"), "{err}");
        assert!(err.contains(OTHER_1) && err.contains(OTHER_2), "{err}");
    }

    #[test]
    fn get_grouped_without_group_lists_groups_and_outputs() {
        let server = MockServer::start();
        serve_index(&server);
        serve_file(
            &server,
            &format!("{HP}_1.202610.xml"),
            &heat_pump_xml(1, "2026-10-01 02:00:00").replace("Consumed,Delivered", "Grid,Solar"),
        );
        serve_file(
            &server,
            &format!("{HP}_2.202611.xml"),
            &heat_pump_xml(2, "2026-11-01 02:00:00"),
        );
        let err = run_get(
            &server,
            &run_ctx(false, false),
            "Heat Pump",
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--group"), "{err}");
        assert!(err.contains("group 1: Grid, Solar"), "{err}");
        assert!(err.contains("group 2: Consumed, Delivered"), "{err}");
    }

    #[test]
    fn get_unknown_name_is_not_found() {
        let server = MockServer::start();
        serve_index(&server);
        let err = run_get(
            &server,
            &run_ctx(false, false),
            "Garage Door",
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Statistic 'Garage Door' not found"), "{err}");
    }

    #[test]
    fn get_http_401_surfaces_as_http_status_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/stats/");
            then.status(401);
        });
        let err =
            run_get(&server, &run_ctx(false, false), "Kitchen", None, None, None).unwrap_err();
        let http = err
            .downcast_ref::<HttpStatusError>()
            .expect("HttpStatusError");
        assert_eq!(http.status, 401);
    }

    fn run_list(
        server: &MockServer,
        filter: Option<&str>,
        month: Option<&str>,
        latest: bool,
    ) -> Vec<StatEntry> {
        let lox = mock_client(server);
        let mut out = Vec::new();
        list(&run_ctx(true, false), &lox, filter, month, latest, &mut out).unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_slice(&out).unwrap();
        v.into_iter()
            .map(|e| StatEntry {
                file: e["file"].as_str().unwrap().into(),
                uuid: e["uuid"].as_str().unwrap().into(),
                group: e["group"].as_u64().map(|g| g as u32),
                month: e["month"].as_str().unwrap().into(),
                name: e["name"].as_str().unwrap().into(),
                label: e["label"].as_str().unwrap().into(),
            })
            .collect()
    }

    #[test]
    fn list_filter_is_case_insensitive_on_label() {
        let server = MockServer::start();
        serve_index(&server);
        let all = run_list(&server, None, None, false);
        assert_eq!(all.len(), 9);
        let garage = run_list(&server, Some("GARAGE"), None, false);
        assert_eq!(garage.len(), 1);
        assert_eq!(garage[0].uuid, OTHER_2);
        let oct = run_list(&server, Some("kitchen"), Some("2026-10"), false);
        assert_eq!(oct.len(), 1);
        assert_eq!(oct[0].file, format!("{KITCHEN}.202610.xml"));
    }

    #[test]
    fn list_latest_keeps_newest_month_per_uuid_and_group() {
        let server = MockServer::start();
        serve_index(&server);
        let latest = run_list(&server, None, None, true);
        let files: Vec<&str> = latest.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(
            files,
            vec![
                format!("{HP}_1.202610.xml"),
                format!("{HP}_2.202611.xml"),
                format!("{KITCHEN}.202610.xml"),
                format!("{OTHER_1}.202610.xml"),
                format!("{OTHER_2}.202610.xml"),
            ]
        );
    }
}
