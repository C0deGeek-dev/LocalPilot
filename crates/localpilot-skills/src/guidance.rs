//! Optional applicability metadata for task guidance (`[guidance]` in
//! `skill.toml`).
//!
//! A skill package can say which tasks its instructions are for: the user's
//! intent, the stack and product of the component being worked on, the paths
//! it applies to, and which capabilities a claim made under it needs. Selection
//! reads this to choose guidance; nothing else does.
//!
//! The section is additive and versioned. A package without it loads and ranks
//! exactly as before. A section that cannot be read never hides the package:
//! the manifest is parsed without it, and the section is reported as invalid
//! with a reason a person can act on. The section carries data only: it has no
//! field that grants a permission, names a tool to allow, or runs anything, and
//! an unknown key is an error rather than silently ignored, so a typo or an
//! attempt to add such a field is visible. The legacy `[triggers]` fields keep
//! their meaning; nothing here reads them.

use std::path::{Component, Path};

use serde::Deserialize;

/// The `schema_version` values this build reads.
pub const GUIDANCE_SCHEMA_VERSION: u32 = 1;

/// What the user asked for.
pub const INTENTS: &[&str] = &[
    "explain",
    "investigate",
    "implement",
    "refactor",
    "review",
    "measure",
    "package",
];

/// What kind of thing the component is.
pub const PRODUCTS: &[&str] = &[
    "library", "cli", "tui", "service", "desktop", "browser", "game",
];

/// Observation capabilities a claim can depend on.
pub const CAPABILITIES: &[&str] = &[
    "process.capture",
    "process.background",
    "terminal.interact",
    "http.observe",
    "browser.interact",
    "image.inspect",
    "desktop.interact",
    "engine.run",
];

/// Whether a package is a primary procedure or a supplement to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceRole {
    /// One task procedure: how to explain, investigate, implement, ...
    Procedure,
    /// Stack or product specifics added beside a procedure.
    Supplement,
}

/// A section of the package a selection may load instead of the whole body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodySection {
    /// A heading in `SKILL.md`, without its `#` marks.
    Heading(String),
    /// A file inside the package directory, relative to it with `/`.
    File(String),
}

/// A valid `[guidance]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceMetadata {
    pub role: GuidanceRole,
    pub intents: Vec<String>,
    pub exclude_intents: Vec<String>,
    pub stacks: Vec<String>,
    pub products: Vec<String>,
    pub component_globs: Vec<String>,
    pub capabilities_optional: Vec<String>,
    pub capabilities_required_for_claim: Vec<String>,
    pub body_sections: Vec<BodySection>,
}

/// What a package says about its applicability.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Applicability {
    /// No `[guidance]` section: ordinary description-based discovery only.
    #[default]
    Absent,
    Valid(GuidanceMetadata),
    /// A section that cannot be used, with why. The package itself is still
    /// listed, searched and loadable as before.
    Invalid(String),
}

impl Applicability {
    /// The metadata, when valid.
    #[must_use]
    pub fn metadata(&self) -> Option<&GuidanceMetadata> {
        match self {
            Self::Valid(metadata) => Some(metadata),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGuidance {
    schema_version: Option<i64>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    intents: Vec<String>,
    #[serde(default)]
    exclude_intents: Vec<String>,
    #[serde(default)]
    stacks: Vec<String>,
    #[serde(default)]
    products: Vec<String>,
    #[serde(default)]
    component_globs: Vec<String>,
    #[serde(default)]
    capabilities_optional: Vec<String>,
    #[serde(default)]
    capabilities_required_for_claim: Vec<String>,
    #[serde(default)]
    body_sections: Vec<String>,
}

/// Read the `[guidance]` section of a `skill.toml` in `package_dir`, whose
/// `SKILL.md` body is `instructions`.
#[must_use]
pub fn read_applicability(manifest: &str, package_dir: &Path, instructions: &str) -> Applicability {
    let manifest = manifest.strip_prefix('\u{feff}').unwrap_or(manifest);
    let Ok(table) = manifest.parse::<toml::Table>() else {
        // The manifest itself parsed elsewhere; a TOML error here would have
        // failed that parse too, so there is no section to report on.
        return Applicability::Absent;
    };
    let Some(section) = table.get("guidance") else {
        return Applicability::Absent;
    };
    match parse(section.clone(), package_dir, instructions) {
        Ok(metadata) => Applicability::Valid(metadata),
        Err(reason) => Applicability::Invalid(reason),
    }
}

fn parse(
    section: toml::Value,
    package_dir: &Path,
    instructions: &str,
) -> Result<GuidanceMetadata, String> {
    if !section.is_table() {
        return Err("[guidance] must be a table".to_string());
    }
    let raw: RawGuidance = section
        .try_into()
        .map_err(|err: toml::de::Error| format!("[guidance] {}", one_line(&err.to_string())))?;
    match raw.schema_version {
        None => return Err("[guidance] needs schema_version = 1".to_string()),
        Some(version) if version == i64::from(GUIDANCE_SCHEMA_VERSION) => {}
        Some(version) => {
            return Err(format!(
                "[guidance] schema_version {version} is not supported; this build reads {GUIDANCE_SCHEMA_VERSION}"
            ))
        }
    }
    let role = match raw.role.as_deref() {
        None | Some("procedure") => GuidanceRole::Procedure,
        Some("supplement") => GuidanceRole::Supplement,
        Some(other) => {
            return Err(format!(
                "[guidance] role \"{other}\" is not one of procedure, supplement"
            ))
        }
    };
    check_vocabulary("intents", &raw.intents, INTENTS)?;
    check_vocabulary("exclude_intents", &raw.exclude_intents, INTENTS)?;
    check_vocabulary("products", &raw.products, PRODUCTS)?;
    check_vocabulary(
        "capabilities_optional",
        &raw.capabilities_optional,
        CAPABILITIES,
    )?;
    check_vocabulary(
        "capabilities_required_for_claim",
        &raw.capabilities_required_for_claim,
        CAPABILITIES,
    )?;
    if let Some(both) = raw.intents.iter().find(|i| raw.exclude_intents.contains(i)) {
        return Err(format!(
            "[guidance] intent \"{both}\" is both in intents and in exclude_intents"
        ));
    }
    if role == GuidanceRole::Procedure && raw.intents.is_empty() {
        return Err("[guidance] a procedure names at least one intent".to_string());
    }
    for stack in &raw.stacks {
        if stack.is_empty()
            || !stack
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
        {
            return Err(format!(
                "[guidance] stack \"{stack}\" must be lowercase letters, digits, dots or hyphens"
            ));
        }
    }
    for glob in &raw.component_globs {
        if globset::Glob::new(glob).is_err() {
            return Err(format!(
                "[guidance] component glob \"{glob}\" is not a valid glob"
            ));
        }
        if escapes(glob) {
            return Err(format!(
                "[guidance] component glob \"{glob}\" must be relative to the workspace"
            ));
        }
    }
    let body_sections = raw
        .body_sections
        .iter()
        .map(|section| resolve_section(section, package_dir, instructions))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GuidanceMetadata {
        role,
        intents: raw.intents,
        exclude_intents: raw.exclude_intents,
        stacks: raw.stacks,
        products: raw.products,
        component_globs: raw.component_globs,
        capabilities_optional: raw.capabilities_optional,
        capabilities_required_for_claim: raw.capabilities_required_for_claim,
        body_sections,
    })
}

fn check_vocabulary(field: &str, values: &[String], allowed: &[&str]) -> Result<(), String> {
    match values
        .iter()
        .find(|value| !allowed.contains(&value.as_str()))
    {
        Some(value) => Err(format!(
            "[guidance] {field} has \"{value}\"; known values are {}",
            allowed.join(", ")
        )),
        None => Ok(()),
    }
}

/// A body section: `#Heading` in `SKILL.md`, or a file inside the package.
fn resolve_section(
    section: &str,
    package_dir: &Path,
    instructions: &str,
) -> Result<BodySection, String> {
    if let Some(heading) = section.strip_prefix('#') {
        let heading = heading.trim();
        if section_text(instructions, heading).is_none() {
            return Err(format!(
                "[guidance] body section \"{section}\" names no heading in SKILL.md"
            ));
        }
        return Ok(BodySection::Heading(heading.to_string()));
    }
    if section.is_empty() || escapes(section) {
        return Err(format!(
            "[guidance] body section \"{section}\" must be a file inside the package"
        ));
    }
    let path = package_dir.join(section);
    // Resolve links too: a file inside the package that points outside it is
    // outside it.
    let inside = match (path.canonicalize(), package_dir.canonicalize()) {
        (Ok(file), Ok(dir)) => file.starts_with(dir) && file.is_file(),
        _ => false,
    };
    if !inside {
        return Err(format!(
            "[guidance] body section \"{section}\" is not a file inside the package"
        ));
    }
    Ok(BodySection::File(section.replace('\\', "/")))
}

/// Whether a relative reference could leave its base: absolute, a drive or
/// root prefix, or a `..` step.
fn escapes(reference: &str) -> bool {
    let path = Path::new(reference);
    reference.starts_with('/')
        || reference.starts_with('\\')
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

/// The text under the Markdown heading `heading` in `body`, up to the next
/// heading of the same or a higher level, heading line included.
#[must_use]
pub fn section_text<'a>(body: &'a str, heading: &str) -> Option<&'a str> {
    let mut start = None;
    let mut level = 0;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end();
        let marks = trimmed.chars().take_while(|c| *c == '#').count();
        let is_heading = marks > 0 && trimmed[marks..].starts_with(' ');
        if let Some(begin) = start {
            if is_heading && marks <= level {
                return Some(&body[begin..offset]);
            }
        } else if is_heading && trimmed[marks..].trim() == heading {
            start = Some(offset);
            level = marks;
        }
        offset += line.len();
    }
    start.map(|begin| &body[begin..])
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const BODY: &str = "Intro.\n\n## Steps\nDo one.\n### Detail\nMore.\n## After\nDone.\n";

    fn read(section: &str) -> Applicability {
        let dir = tempfile::tempdir().unwrap();
        read_applicability(
            &format!("name = \"x\"\ndescription = \"d\"\nversion = \"1.0.0\"\n{section}"),
            dir.path(),
            BODY,
        )
    }

    fn invalid(section: &str) -> String {
        match read(section) {
            Applicability::Invalid(reason) => reason,
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_manifest_without_the_section_is_absent() {
        assert_eq!(read(""), Applicability::Absent);
    }

    #[test]
    fn a_full_section_is_read() {
        let found = read(
            "[guidance]\nschema_version = 1\nrole = \"supplement\"\nstacks = [\"rust\"]\n\
             products = [\"cli\"]\ncomponent_globs = [\"crates/**\"]\n\
             capabilities_required_for_claim = [\"process.capture\"]\n\
             body_sections = [\"#Steps\"]\n",
        );
        let metadata = found.metadata().unwrap();
        assert_eq!(metadata.role, GuidanceRole::Supplement);
        assert_eq!(metadata.stacks, ["rust"]);
        assert_eq!(
            metadata.body_sections,
            [BodySection::Heading("Steps".into())]
        );
    }

    #[test]
    fn each_unusable_section_says_why() {
        assert!(invalid("[guidance]\nintents = [\"implement\"]\n").contains("schema_version = 1"));
        assert!(
            invalid("[guidance]\nschema_version = 2\nintents = [\"implement\"]\n")
                .contains("not supported")
        );
        assert!(
            invalid("[guidance]\nschema_version = 1\nintents = [\"deploy\"]\n")
                .contains("known values are")
        );
        assert!(invalid("[guidance]\nschema_version = 1\n").contains("at least one intent"));
        assert!(invalid(
            "[guidance]\nschema_version = 1\nintents = [\"review\"]\nexclude_intents = [\"review\"]\n"
        )
        .contains("both"));
        assert!(invalid(
            "[guidance]\nschema_version = 1\nintents = [\"review\"]\ncomponent_globs = [\"../**\"]\n"
        )
        .contains("relative"));
        assert!(invalid(
            "[guidance]\nschema_version = 1\nintents = [\"review\"]\nbody_sections = [\"#Missing\"]\n"
        )
        .contains("no heading"));
        assert!(invalid(
            "[guidance]\nschema_version = 1\nintents = [\"review\"]\nbody_sections = [\"../x.md\"]\n"
        )
        .contains("inside the package"));
        assert!(invalid("guidance = 3\n").contains("table"));
    }

    #[test]
    fn a_field_that_would_grant_something_is_rejected_not_ignored() {
        for field in [
            "permissions = [\"net:*\"]",
            "tools = [\"run_shell\"]",
            "hooks = [\"pre\"]",
        ] {
            let reason = invalid(&format!(
                "[guidance]\nschema_version = 1\nintents = [\"review\"]\n{field}\n"
            ));
            assert!(reason.contains("unknown field"), "{field}: {reason}");
        }
    }

    #[test]
    fn a_heading_section_runs_to_the_next_heading_of_its_level() {
        assert_eq!(
            section_text(BODY, "Steps"),
            Some("## Steps\nDo one.\n### Detail\nMore.\n")
        );
        assert_eq!(section_text(BODY, "After"), Some("## After\nDone.\n"));
        assert_eq!(section_text(BODY, "Nope"), None);
    }
}
