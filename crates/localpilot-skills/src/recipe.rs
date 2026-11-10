//! Reviewed project run recipes (`[[recipe]]` in a project's `skill.toml`).
//!
//! A generic Rust, .NET, browser or game procedure cannot know how one
//! repository builds, starts, observes and stops its application. A recipe
//! written and reviewed by the project says so: the working directory, the
//! program and its arguments, the component it is for, prerequisites, the
//! names of the environment variables or credentials it needs, how readiness
//! shows, a scenario to exercise and who cleans up.
//!
//! A recipe is advice for the model. It is never run by the host: commands go
//! through the ordinary tools and their permission checks, and a recipe has no
//! field that grants anything or replaces a configured check. It cannot hold a
//! secret: environment entries are names only, and arguments that look like a
//! credential make the recipe invalid. A recipe may name the files its
//! validity depends on with their hashes; when one has changed, the recipe is
//! reported as not validated for the current state. An unusable recipe is
//! reported and never hides its package.

use std::path::{Component, Path};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// How readiness shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// The program's output contains this text.
    OutputContains(String),
    /// An HTTP GET to this local URL succeeds.
    HttpGet(String),
    /// The program exits with code 0 (a one-shot command).
    ExitZero,
}

/// Who stops what the recipe starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    /// Stop exactly the process this recipe started.
    StopStartedProcess,
    /// Nothing keeps running.
    Nothing,
}

/// One reviewed recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub name: String,
    /// The component it is for, workspace-relative with `/`; `.` for the root.
    pub component: String,
    /// Where to run, relative to the workspace.
    pub working_directory: String,
    pub program: String,
    pub args: Vec<String>,
    pub prerequisites: Vec<String>,
    /// Names of environment variables the recipe needs. Never values.
    pub env: Vec<String>,
    /// Names of credentials the user must supply. Never values.
    pub credentials: Vec<String>,
    pub readiness: Readiness,
    pub scenario: Vec<String>,
    pub cleanup: Cleanup,
    /// Workspace-relative files and the SHA-256 they had when the recipe was
    /// last validated.
    pub validated_against: Vec<(String, String)>,
}

/// Whether a recipe is known to hold for the project as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeValidation {
    /// Every file it was validated against is unchanged.
    Current,
    /// These files changed or are missing since validation.
    Stale(Vec<String>),
    /// It names no files to validate against.
    Unvalidated,
}

impl Recipe {
    /// Compare the recorded hashes with the files under `root`.
    #[must_use]
    pub fn validation(&self, root: &Path) -> RecipeValidation {
        if self.validated_against.is_empty() {
            return RecipeValidation::Unvalidated;
        }
        let changed: Vec<String> = self
            .validated_against
            .iter()
            .filter(|(file, hash)| file_sha256(&root.join(file)).as_deref() != Some(hash.as_str()))
            .map(|(file, _)| file.clone())
            .collect();
        if changed.is_empty() {
            RecipeValidation::Current
        } else {
            RecipeValidation::Stale(changed)
        }
    }

    /// One line for a reader or a model.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut command = self.program.clone();
        for arg in &self.args {
            command.push(' ');
            command.push_str(arg);
        }
        let readiness = match &self.readiness {
            Readiness::OutputContains(text) => format!("ready when the output contains \"{text}\""),
            Readiness::HttpGet(url) => format!("ready when GET {url} succeeds"),
            Readiness::ExitZero => "done when it exits with code 0".to_string(),
        };
        let mut line = format!("run `{command}` in {}; {readiness}", self.working_directory);
        if !self.scenario.is_empty() {
            line.push_str(&format!("; then {}", self.scenario.join("; ")));
        }
        if self.cleanup == Cleanup::StopStartedProcess {
            line.push_str("; stop the process it started when done");
        }
        if !self.env.is_empty() || !self.credentials.is_empty() {
            let mut needs: Vec<&str> = self.env.iter().map(String::as_str).collect();
            needs.extend(self.credentials.iter().map(String::as_str));
            line.push_str(&format!("; needs {} from the user", needs.join(", ")));
        }
        line
    }
}

/// The recipes a package declares, and the ones that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecipeSet {
    pub recipes: Vec<Recipe>,
    /// `name: reason` for each unusable recipe.
    pub invalid: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecipe {
    name: String,
    #[serde(default = "root")]
    component: String,
    #[serde(default = "root")]
    working_directory: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    prerequisites: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    credentials: Vec<String>,
    readiness: RawReadiness,
    #[serde(default)]
    scenario: Vec<String>,
    cleanup: String,
    #[serde(default)]
    validated_against: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReadiness {
    kind: String,
    #[serde(default)]
    value: Option<String>,
}

fn root() -> String {
    ".".to_string()
}

/// Read the `[[recipe]]` entries of a `skill.toml`.
#[must_use]
pub fn read_recipes(manifest: &str) -> RecipeSet {
    let manifest = manifest.strip_prefix('\u{feff}').unwrap_or(manifest);
    let Ok(table) = manifest.parse::<toml::Table>() else {
        return RecipeSet::default();
    };
    let Some(entries) = table.get("recipe") else {
        return RecipeSet::default();
    };
    let Some(entries) = entries.as_array() else {
        return RecipeSet {
            recipes: Vec::new(),
            invalid: vec!["recipe: use [[recipe]] entries".to_string()],
        };
    };
    let mut set = RecipeSet::default();
    for (index, entry) in entries.iter().enumerate() {
        let label = entry
            .get("name")
            .and_then(toml::Value::as_str)
            .map_or_else(|| format!("recipe {}", index + 1), str::to_string);
        match parse(entry.clone()) {
            Ok(recipe) => set.recipes.push(recipe),
            Err(reason) => set.invalid.push(format!("{label}: {reason}")),
        }
    }
    set
}

fn parse(entry: toml::Value) -> Result<Recipe, String> {
    let raw: RawRecipe = entry.try_into().map_err(|err: toml::de::Error| {
        err.to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    })?;
    for (field, value) in [
        ("component", &raw.component),
        ("working_directory", &raw.working_directory),
    ] {
        if escapes(value) {
            return Err(format!(
                "{field} \"{value}\" must be relative to the workspace"
            ));
        }
    }
    if raw.program.trim().is_empty() {
        return Err("program is empty".to_string());
    }
    for name in raw.env.iter().chain(&raw.credentials) {
        if !is_variable_name(name) {
            return Err(format!(
                "\"{name}\" is not a variable name; recipes name variables and never hold values"
            ));
        }
    }
    for arg in std::iter::once(&raw.program).chain(&raw.args) {
        if looks_like_secret(arg) {
            return Err(
                "an argument looks like a credential; name it in `credentials` instead".to_string(),
            );
        }
    }
    let readiness = match (raw.readiness.kind.as_str(), raw.readiness.value) {
        ("output_contains", Some(text)) if !text.is_empty() => Readiness::OutputContains(text),
        ("http_get", Some(url)) if is_local_url(&url) => Readiness::HttpGet(url),
        ("http_get", Some(url)) => {
            return Err(format!("readiness URL \"{url}\" must be on localhost"))
        }
        ("exit_zero", None) => Readiness::ExitZero,
        (kind, _) => {
            return Err(format!(
                "readiness kind \"{kind}\" needs one of output_contains (with value), http_get (with a local URL) or exit_zero"
            ))
        }
    };
    let cleanup = match raw.cleanup.as_str() {
        "stop_started_process" => Cleanup::StopStartedProcess,
        "nothing" => Cleanup::Nothing,
        other => {
            return Err(format!(
                "cleanup \"{other}\" is not one of stop_started_process, nothing"
            ))
        }
    };
    let mut validated_against = Vec::new();
    for (file, hash) in raw.validated_against {
        if escapes(&file) {
            return Err(format!(
                "validated_against file \"{file}\" must be relative"
            ));
        }
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "validated_against hash for \"{file}\" is not a SHA-256"
            ));
        }
        validated_against.push((file, hash.to_ascii_lowercase()));
    }
    Ok(Recipe {
        name: raw.name,
        component: normalize(&raw.component),
        working_directory: normalize(&raw.working_directory),
        program: raw.program,
        args: raw.args,
        prerequisites: raw.prerequisites,
        env: raw.env,
        credentials: raw.credentials,
        readiness,
        scenario: raw.scenario,
        cleanup,
        validated_against,
    })
}

/// A recipe draft from a command that ran successfully, for a person to review
/// and add to a project package. Nothing is written.
#[must_use]
pub fn draft_recipe(
    name: &str,
    component: &str,
    program: &str,
    args: &[String],
    root: &Path,
    depends_on: &[&str],
) -> String {
    let quote = |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
    let mut out = String::from(
        "# Draft from a successful run. Review it, then add it to a project skill.toml.\n[[recipe]]\n",
    );
    out.push_str(&format!("name = {}\n", quote(name)));
    out.push_str(&format!("component = {}\n", quote(component)));
    out.push_str(&format!("working_directory = {}\n", quote(component)));
    out.push_str(&format!("program = {}\n", quote(program)));
    let args: Vec<String> = args
        .iter()
        .map(|arg| {
            if looks_like_secret(arg) {
                quote("<credential: name it in credentials>")
            } else {
                quote(arg)
            }
        })
        .collect();
    out.push_str(&format!("args = [{}]\n", args.join(", ")));
    out.push_str("readiness = { kind = \"exit_zero\" }\ncleanup = \"nothing\"\n");
    let hashes: Vec<String> = depends_on
        .iter()
        .filter_map(|file| {
            file_sha256(&root.join(file)).map(|hash| format!("{} = {}", quote(file), quote(&hash)))
        })
        .collect();
    if !hashes.is_empty() {
        out.push_str(&format!(
            "validated_against = {{ {} }}\n",
            hashes.join(", ")
        ));
    }
    out
}

fn file_sha256(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

fn normalize(path: &str) -> String {
    let trimmed = path.replace('\\', "/");
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "./" {
        ".".to_string()
    } else {
        trimmed.trim_start_matches("./").to_string()
    }
}

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

fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_local_url(url: &str) -> bool {
    ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|prefix| url.starts_with(prefix))
}

/// Whether an argument looks like it carries a credential.
fn looks_like_secret(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "sk-",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "akia",
        "glpat-",
    ];
    if PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix) && arg.len() > prefix.len() + 8)
    {
        return true;
    }
    const KEYS: &[&str] = &[
        "password=",
        "passwd=",
        "token=",
        "secret=",
        "api_key=",
        "apikey=",
        "api-key=",
    ];
    if KEYS.iter().any(|key| lower.contains(key)) {
        return true;
    }
    // user:password@ in a URL
    if let Some(rest) = lower.split("://").nth(1) {
        if let Some(userinfo) = rest.split('@').next().filter(|_| rest.contains('@')) {
            return userinfo.contains(':');
        }
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const GOOD: &str = r#"
name = "x"
description = "d"
version = "1.0.0"

[[recipe]]
name = "api-dev"
component = "api"
working_directory = "api"
program = "dotnet"
args = ["run", "--urls", "http://127.0.0.1:5199"]
env = ["ASPNETCORE_ENVIRONMENT"]
credentials = ["ORDERS_DB_PASSWORD"]
readiness = { kind = "http_get", value = "http://127.0.0.1:5199/health" }
scenario = ["POST an order", "GET it back"]
cleanup = "stop_started_process"
"#;

    #[test]
    fn a_reviewed_recipe_is_read() {
        let set = read_recipes(GOOD);
        assert!(set.invalid.is_empty(), "{:?}", set.invalid);
        let recipe = &set.recipes[0];
        assert_eq!(recipe.component, "api");
        assert_eq!(
            recipe.readiness,
            Readiness::HttpGet("http://127.0.0.1:5199/health".into())
        );
        assert_eq!(recipe.cleanup, Cleanup::StopStartedProcess);
        let summary = recipe.summary();
        assert!(summary.contains("dotnet run --urls"), "{summary}");
        assert!(summary.contains("needs ASPNETCORE_ENVIRONMENT, ORDERS_DB_PASSWORD from the user"));
    }

    fn reason(recipe: &str) -> String {
        let set = read_recipes(&format!(
            "name = \"x\"\ndescription = \"d\"\nversion = \"1\"\n[[recipe]]\n{recipe}"
        ));
        assert!(set.recipes.is_empty());
        set.invalid.join("; ")
    }

    #[test]
    fn a_recipe_cannot_hold_a_secret_grant_anything_or_leave_the_workspace() {
        let base = "name = \"r\"\nprogram = \"app\"\nreadiness = { kind = \"exit_zero\" }\ncleanup = \"nothing\"\n";
        assert!(reason(&format!("{base}args = [\"--token=abc123\"]\n")).contains("credential"));
        assert!(reason(&format!("{base}args = [\"sk-abcdefghijklmnop\"]\n")).contains("credential"));
        assert!(reason(&format!(
            "{base}args = [\"postgres://me:pw@localhost/db\"]\n"
        ))
        .contains("credential"));
        assert!(reason(&format!("{base}env = [\"API_KEY=abc\"]\n")).contains("never hold values"));
        assert!(reason(&format!("{base}permissions = [\"net\"]\n")).contains("unknown field"));
        assert!(reason(&format!("{base}check = \"replace\"\n")).contains("unknown field"));
        assert!(reason(&format!("{base}working_directory = \"../other\"\n")).contains("relative"));
        assert!(reason("name = \"r\"\nprogram = \"app\"\nreadiness = { kind = \"http_get\", value = \"https://example.com\" }\ncleanup = \"nothing\"\n").contains("localhost"));
        assert!(reason("name = \"r\"\nprogram = \"app\"\nreadiness = { kind = \"exit_zero\" }\ncleanup = \"leave it\"\n").contains("cleanup"));
    }

    #[test]
    fn a_recipe_is_stale_once_a_file_it_was_validated_against_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "v1").unwrap();
        let draft = draft_recipe(
            "run",
            ".",
            "cargo",
            &["run".to_string()],
            dir.path(),
            &["Cargo.lock"],
        );
        let manifest = format!("name = \"x\"\ndescription = \"d\"\nversion = \"1\"\n{draft}");
        let recipe = read_recipes(&manifest).recipes.remove(0);
        assert_eq!(recipe.validation(dir.path()), RecipeValidation::Current);
        std::fs::write(dir.path().join("Cargo.lock"), "v2").unwrap();
        assert_eq!(
            recipe.validation(dir.path()),
            RecipeValidation::Stale(vec!["Cargo.lock".into()])
        );
        let unvalidated = read_recipes(GOOD).recipes.remove(0);
        assert_eq!(
            unvalidated.validation(dir.path()),
            RecipeValidation::Unvalidated
        );
    }

    #[test]
    fn a_draft_masks_a_credential_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let before: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        let draft = draft_recipe(
            "seed",
            "api",
            "tool",
            &["--password=hunter22".to_string()],
            dir.path(),
            &[],
        );
        assert!(!draft.contains("hunter22"));
        assert!(draft.starts_with("# Draft"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), before.len());
    }
}
