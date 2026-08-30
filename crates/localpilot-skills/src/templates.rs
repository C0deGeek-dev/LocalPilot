//! Parameterized prompt templates.
//!
//! Deliberately small and distinct from skills: a template is one markdown
//! file whose body may contain `{{parameter}}` placeholders, rendered with
//! caller-supplied values. User-scoped and project-scoped directories are both
//! plain template directories; project-local templates load only behind the
//! workspace trust gate (the caller enforces it).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::SkillError;

/// One reusable prompt: a name (the file stem unless frontmatter overrides
/// it), an optional description, and a parameterized body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: Option<String>,
    pub body: String,
}

impl PromptTemplate {
    /// Render the body, substituting every `{{parameter}}` placeholder from
    /// `params`. Strict: an unresolved placeholder is an error naming the
    /// missing parameters, never silently passed through.
    ///
    /// # Errors
    /// Returns [`SkillError::InvalidTemplate`] listing unresolved parameters.
    pub fn render(&self, params: &BTreeMap<String, String>) -> Result<String, SkillError> {
        let mut out = self.body.clone();
        for (key, value) in params {
            out = out.replace(&format!("{{{{{key}}}}}"), value);
        }
        let unresolved = placeholders(&out);
        if unresolved.is_empty() {
            Ok(out)
        } else {
            Err(SkillError::InvalidTemplate(format!(
                "template `{}` has unresolved parameters: {}",
                self.name,
                unresolved.join(", ")
            )))
        }
    }

    /// The parameter names this template expects.
    #[must_use]
    pub fn parameters(&self) -> Vec<String> {
        placeholders(&self.body)
    }
}

/// All `{{name}}` placeholders in `text`, in order, deduplicated.
fn placeholders(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        let name = after[..end].trim();
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && !found.iter().any(|f| f == name)
        {
            found.push(name.to_string());
        }
        rest = &after[end + 2..];
    }
    found
}

/// A set of discovered prompt templates.
#[derive(Debug, Clone, Default)]
pub struct TemplateSet {
    templates: Vec<PromptTemplate>,
}

impl TemplateSet {
    /// Load every `*.md` file in each directory as a template. An optional
    /// YAML frontmatter block may set `name` and `description`; otherwise the
    /// file stem is the name. Later directories do not override earlier ones.
    ///
    /// # Errors
    /// Returns [`SkillError`] if a template file cannot be read or its
    /// frontmatter fails to parse.
    pub fn load(dirs: &[PathBuf]) -> Result<Self, SkillError> {
        let mut templates = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
                .collect();
            paths.sort();
            for path in paths {
                templates.push(parse_template(&path)?);
            }
        }
        Ok(Self { templates })
    }

    /// The names of all loaded templates.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.templates.iter().map(|t| t.name.as_str()).collect()
    }

    /// Find a template by exact name.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&PromptTemplate> {
        self.templates.iter().find(|t| t.name == name)
    }
}

/// The user-scoped and project-scoped template directories, in load order.
#[must_use]
pub fn standard_template_dirs(project_root: &Path) -> Vec<PathBuf> {
    vec![project_root.join(".localpilot").join("prompts")]
}

#[derive(Debug, Default, serde::Deserialize)]
struct TemplateFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

fn parse_template(path: &Path) -> Result<PromptTemplate, SkillError> {
    let content = std::fs::read_to_string(path).map_err(|source| SkillError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (front, body) = match content
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---"))
    {
        Some((front, body)) => {
            let front: TemplateFrontmatter = serde_yaml::from_str(front)
                .map_err(|e| SkillError::InvalidTemplate(e.to_string()))?;
            (front, body.trim_start_matches('\n').to_string())
        }
        None => (TemplateFrontmatter::default(), content),
    };
    Ok(PromptTemplate {
        name: front.name.unwrap_or(stem),
        description: front.description,
        body,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn loads_and_renders_a_parameterized_template() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("review.md"),
            "---\ndescription: review a file\n---\nReview {{path}} focusing on {{focus}}.\n",
        )
        .unwrap();

        let set = TemplateSet::load(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(set.names(), vec!["review"]);
        let template = set.by_name("review").unwrap();
        assert_eq!(template.parameters(), vec!["path", "focus"]);
        let rendered = template
            .render(&params(&[("path", "src/lib.rs"), ("focus", "errors")]))
            .unwrap();
        assert_eq!(rendered.trim(), "Review src/lib.rs focusing on errors.");
    }

    #[test]
    fn a_missing_parameter_is_a_named_error_not_a_passthrough() {
        let template = PromptTemplate {
            name: "t".to_string(),
            description: None,
            body: "Do {{thing}} with {{tool}}".to_string(),
        };
        let err = template.render(&params(&[("thing", "x")])).unwrap_err();
        match err {
            SkillError::InvalidTemplate(message) => {
                assert!(message.contains("tool"), "{message}");
            }
            other => panic!("expected InvalidTemplate, got {other:?}"),
        }
    }

    #[test]
    fn a_template_without_frontmatter_uses_the_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("explain.md"), "Explain {{topic}} simply.\n").unwrap();
        let set = TemplateSet::load(&[dir.path().to_path_buf()]).unwrap();
        let template = set.by_name("explain").unwrap();
        assert!(template.description.is_none());
        assert_eq!(template.parameters(), vec!["topic"]);
    }
}
