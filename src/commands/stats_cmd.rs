//! `lox stats` — read the statistics the Miniserver logs to its SD card.
//!
//! The Miniserver serves an HTML index at `/stats/` with one entry per
//! statistic and month, and each month as a self-describing XML file
//! (`/stats/<uuid>[_<group>].<YYYYMM>.xml`). Everything here is read-only.

use anyhow::{Context, Result, bail};
use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, Event};
use serde::Serialize;
use std::collections::HashSet;

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
}
