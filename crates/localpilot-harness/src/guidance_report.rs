//! What task guidance is active, and why other guidance is not.
//!
//! The report joins two host records: the last guidance selection (what was
//! chosen, suggested and left out, with reasons) and the guidance record of
//! the last provider request (what the request actually held). A selected
//! package is only reported active when the request held it; otherwise the
//! report says so. Guidance that reached the request by another route, such
//! as project instructions or a skill the model loaded, is listed too.
//!
//! The report shows host decisions and observed facts. It holds no model
//! output, and building it changes nothing.

use std::fmt::Write as _;

use localpilot_core::{GuidanceForm, GuidanceKind};

use crate::{
    GuidanceDelivery, GuidanceSelection, LoadDecision, RequestGuidance, SelectionConfidence,
};

/// The state of one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceStatus {
    /// In the last provider request, whole.
    Active,
    /// Chosen, but the last request did not hold it whole: not sent yet, cut,
    /// or removed by a compaction.
    Stale,
    /// Offered by name and summary only.
    Suggested,
    /// Considered and left out.
    Excluded,
    /// A claim the guidance supports needs a capability this session lacks.
    Unavailable,
}

impl GuidanceStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Stale => "stale",
            Self::Suggested => "suggested",
            Self::Excluded => "excluded",
            Self::Unavailable => "unavailable",
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceRow {
    pub name: String,
    pub status: GuidanceStatus,
    pub detail: String,
}

/// The report for a session at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuidanceReport {
    /// The intent, component and loading decision, when a selection ran.
    pub header: Vec<String>,
    pub rows: Vec<GuidanceRow>,
    /// One line for a status bar.
    pub summary: String,
}

impl GuidanceReport {
    /// The rows with `status`.
    pub fn with_status(&self, status: GuidanceStatus) -> impl Iterator<Item = &GuidanceRow> {
        self.rows.iter().filter(move |row| row.status == status)
    }

    /// The row for `name` with `status`, if any.
    #[must_use]
    pub fn row(&self, name: &str, status: GuidanceStatus) -> Option<&GuidanceRow> {
        self.rows
            .iter()
            .find(|row| row.name == name && row.status == status)
    }

    /// The full report as text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("{}\n", self.summary);
        for line in &self.header {
            let _ = writeln!(out, "{line}");
        }
        for status in [
            GuidanceStatus::Active,
            GuidanceStatus::Stale,
            GuidanceStatus::Unavailable,
            GuidanceStatus::Suggested,
            GuidanceStatus::Excluded,
        ] {
            let rows: Vec<&GuidanceRow> = self.with_status(status).collect();
            if rows.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n{}:", status.label());
            for row in rows {
                let _ = writeln!(out, "  {} — {}", row.name, row.detail);
            }
        }
        out
    }
}

/// Build the report from the last selection and the last request's record.
#[must_use]
pub fn guidance_report(
    selection: Option<&GuidanceSelection>,
    last_request: Option<&RequestGuidance>,
) -> GuidanceReport {
    let mut report = GuidanceReport::default();
    let entries = last_request
        .map(|r| r.entries.as_slice())
        .unwrap_or_default();
    let mut claimed: Vec<String> = Vec::new();

    if let Some(selection) = selection {
        let intent = if selection.intent.phases.is_empty() {
            "none".to_string()
        } else {
            selection
                .intent
                .phases
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(" → ")
        };
        report.header.push(format!(
            "intent: {intent} ({}; {})",
            confidence(selection.intent.confidence),
            selection.intent.reason
        ));
        let mut component = format!(
            "component: {} ({}; {})",
            selection.component.component,
            confidence(selection.component.confidence),
            selection.component.reason
        );
        if !selection.component.alternatives.is_empty() {
            let _ = write!(
                component,
                "; also {}",
                selection.component.alternatives.join(", ")
            );
        }
        report.header.push(component);
        if !selection.stacks.is_empty() || !selection.products.is_empty() {
            let list = |items: &[(String, SelectionConfidence)]| {
                items
                    .iter()
                    .map(|(name, c)| format!("{name} ({})", confidence(*c)))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            report.header.push(format!(
                "evidence: stacks {}; products {}",
                list(&selection.stacks),
                list(&selection.products)
            ));
        }
        for note in &selection.notes {
            report.header.push(format!("note: {note}"));
        }
        report.header.push(format!(
            "loading: {} ({})",
            match selection.loading {
                LoadDecision::Body => "bodies",
                LoadDecision::Metadata => "metadata only",
            },
            selection.loading_reason
        ));

        for selected in selection.primary.iter().chain(&selection.supplements) {
            claimed.push(selected.name.clone());
            let held: Vec<_> = entries
                .iter()
                .filter(|e| {
                    e.source.kind == GuidanceKind::PackageSkill && e.source.id == selected.name
                })
                .collect();
            let whole = held.iter().find(|e| e.delivery == GuidanceDelivery::Intact);
            match whole {
                Some(entry) => report.rows.push(GuidanceRow {
                    name: selected.name.clone(),
                    status: GuidanceStatus::Active,
                    detail: format!(
                        "{}; {} {}, {}",
                        selected.reason,
                        form(entry.source.form),
                        entry.source.origin,
                        placement(entry)
                    ),
                }),
                None => report.rows.push(GuidanceRow {
                    name: selected.name.clone(),
                    status: GuidanceStatus::Stale,
                    detail: format!(
                        "{}; {}",
                        selected.reason,
                        match held.first().map(|e| e.delivery) {
                            Some(GuidanceDelivery::Altered) =>
                                "the last request held only part of it",
                            Some(GuidanceDelivery::Absent) => "no longer in the last request",
                            _ => "not in the last request",
                        }
                    ),
                }),
            }
            if !selected.unsupported_claims.is_empty() {
                report.rows.push(GuidanceRow {
                    name: selected.name.clone(),
                    status: GuidanceStatus::Unavailable,
                    detail: format!(
                        "claims needing {} cannot be made in this session",
                        selected.unsupported_claims.join(", ")
                    ),
                });
            }
        }
        for suggested in &selection.suggestions {
            report.rows.push(GuidanceRow {
                name: suggested.name.clone(),
                status: GuidanceStatus::Suggested,
                detail: suggested.reason.clone(),
            });
        }
        for excluded in &selection.excluded {
            report.rows.push(GuidanceRow {
                name: excluded.name.clone(),
                status: GuidanceStatus::Excluded,
                detail: excluded.reason.clone(),
            });
        }
    }

    // Guidance the request held by other routes.
    for entry in entries {
        if entry.delivery != GuidanceDelivery::Intact {
            continue;
        }
        if entry.source.kind == GuidanceKind::PackageSkill && claimed.contains(&entry.source.id) {
            continue;
        }
        if report
            .rows
            .iter()
            .any(|row| row.status == GuidanceStatus::Active && row.name == entry.source.id)
        {
            continue;
        }
        report.rows.push(GuidanceRow {
            name: entry.source.id.clone(),
            status: GuidanceStatus::Active,
            detail: format!(
                "{}; {} {}, {}",
                entry.source.reason,
                form(entry.source.form),
                entry.source.origin,
                placement(entry)
            ),
        });
    }

    let count = |status| {
        report
            .rows
            .iter()
            .filter(|row| row.status == status)
            .count()
    };
    let mut summary = format!("guidance: {} active", count(GuidanceStatus::Active));
    for (status, word) in [
        (GuidanceStatus::Stale, "stale"),
        (GuidanceStatus::Unavailable, "with unsupported claims"),
        (GuidanceStatus::Suggested, "suggested"),
        (GuidanceStatus::Excluded, "excluded"),
    ] {
        let n = count(status);
        if n > 0 {
            let _ = write!(summary, " · {n} {word}");
        }
    }
    if selection.is_none() {
        summary.push_str(" · no task selection ran");
    }
    report.summary = summary;
    report
}

fn confidence(confidence: SelectionConfidence) -> &'static str {
    match confidence {
        SelectionConfidence::Confirmed => "confirmed",
        SelectionConfidence::Declared => "declared",
        SelectionConfidence::Tentative => "tentative",
    }
}

fn form(form: GuidanceForm) -> &'static str {
    match form {
        GuidanceForm::Metadata => "summary from",
        _ => "body from",
    }
}

fn placement(entry: &crate::GuidanceEntry) -> &'static str {
    match entry.placement {
        crate::GuidancePlacement::System => "in the system text",
        crate::GuidancePlacement::BesideQuestion => "beside the question",
        crate::GuidancePlacement::ToolResult => "in a tool result",
    }
}
