//! Bounded, read-only facts about one component of a project.
//!
//! Task guidance has to fit the project it is used in: a Rust CLI, an ASP.NET
//! Core service beside a web frontend, a Godot game whose generated `.csproj`
//! says nothing about how the game is launched. Root-level inventory is
//! misleading in a mixed repository, so facts here belong to a *component*: the
//! nearest directory, from a path upward, that holds a project marker.
//!
//! Every fact names the file that established it and how sure it is:
//! confirmed by a manifest, declared by the user, or a tentative inference.
//! What cannot be read or is not supported is recorded as unknown with a
//! reason, beside the facts that could be read; nothing here fails a task.
//!
//! Detection only reads. It does not run a package script, a build, an import
//! or a project plugin, and it does not install or download anything. Whether a
//! tool is present is a lookup on `PATH`, never a run of it. Reads are bounded
//! in size, directory listings in length, and no repository-wide scan happens:
//! a component's facts come from its own directory and, for workspace and
//! toolchain pins, its ancestors up to the workspace root.
//!
//! Facts are cached per component against the files they were read from and
//! the directories that were listed. A change to one of those, or an explicit
//! refresh, makes the next request detect again; nothing else does.
//!
//! Nothing here is wired into a session or a prompt. It is the fact layer that
//! task-guidance selection reads.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// The largest manifest or configuration file read, in bytes.
const MAX_READ_BYTES: u64 = 256 * 1024;
/// The largest lockfile read, in bytes. Lockfiles of real projects are large.
const MAX_LOCKFILE_BYTES: u64 = 16 * 1024 * 1024;
/// The most directory entries examined in one directory.
const MAX_DIR_ENTRIES: usize = 512;
/// The most project files of one kind (`.csproj`, ...) read in one component.
const MAX_PROJECT_FILES: usize = 8;
/// How deep the component inventory looks below the root.
const INVENTORY_DEPTH: usize = 2;
/// The most directories the component inventory lists.
const MAX_INVENTORY_DIRS: usize = 256;
/// The longest value kept for a fact, in characters.
const MAX_VALUE_CHARS: usize = 200;

/// Directories that hold dependencies, build output or engine caches. They are
/// never components, and a path inside one belongs to the component above it.
const IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "bin",
    "obj",
    "dist",
    "build",
    "out",
    "vendor",
    "__pycache__",
    "venv",
    "Library",
    "Temp",
];

/// Dependencies of a Node package that say something about the product or the
/// conventions to follow.
const WATCHED_NODE_DEPENDENCIES: &[&str] = &[
    "react",
    "vue",
    "svelte",
    "next",
    "vite",
    "three",
    "@react-three/fiber",
    "tailwindcss",
    "electron",
    "typescript",
];

/// Package scripts reported as existing checks or entry points.
const WATCHED_SCRIPTS: &[&str] = &[
    "test",
    "lint",
    "check",
    "typecheck",
    "build",
    "dev",
    "start",
];

/// How sure a fact is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FactConfidence {
    /// Read from a manifest or configuration file in the project.
    Confirmed,
    /// Said by the user, for example the stack of a project not created yet.
    Declared,
    /// Inferred from evidence that allows another reading.
    Tentative,
}

/// One fact about a component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFact {
    /// A stable dotted name: `cargo.bin`, `aspnet.project`, `godot.version`.
    pub id: String,
    /// A version, a name or a path, when the fact has one.
    pub value: Option<String>,
    pub confidence: FactConfidence,
    /// The workspace-relative file that established it, with `/` separators.
    /// `None` for a declared fact and for a tool found on `PATH`.
    pub source: Option<String>,
}

/// Something that could not be established, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownFact {
    pub id: String,
    pub reason: String,
}

/// The facts of one component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentFacts {
    /// The component directory relative to the workspace root, with `/`
    /// separators; `.` for the root itself.
    pub component: String,
    /// Sorted by id, then value.
    pub facts: Vec<ProjectFact>,
    pub unknown: Vec<UnknownFact>,
    stamps: Vec<Stamp>,
}

impl ComponentFacts {
    /// The first fact named `id`, of any confidence.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ProjectFact> {
        self.facts.iter().find(|fact| fact.id == id)
    }

    /// Whether a fact named `id` is present, of any confidence.
    #[must_use]
    pub fn has(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// The facts with confidence `confidence`.
    pub fn with_confidence(
        &self,
        confidence: FactConfidence,
    ) -> impl Iterator<Item = &ProjectFact> {
        self.facts
            .iter()
            .filter(move |fact| fact.confidence == confidence)
    }

    /// The unknown entry named `id`, if any.
    #[must_use]
    pub fn unknown(&self, id: &str) -> Option<&UnknownFact> {
        self.unknown.iter().find(|unknown| unknown.id == id)
    }

    /// The files and directories these facts depend on, workspace-relative.
    #[must_use]
    pub fn depends_on(&self) -> Vec<String> {
        self.stamps
            .iter()
            .map(|stamp| stamp.relative.clone())
            .collect()
    }
}

/// The state of one file or directory when facts were read from it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    relative: String,
    state: Option<(u64, Option<SystemTime>)>,
}

impl Stamp {
    fn take(root: &Path, path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            relative: relative(root, path).unwrap_or_else(|| path.display().to_string()),
            state: stat(path),
        }
    }

    fn is_current(&self) -> bool {
        stat(&self.path) == self.state
    }
}

fn stat(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    std::fs::metadata(path)
        .ok()
        .map(|meta| (meta.len(), meta.modified().ok()))
}

/// Decides whether a program is on `PATH` without running it.
pub type ToolLookup = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// A cache of component facts for one workspace.
pub struct ProjectFacts {
    root: PathBuf,
    tool_lookup: ToolLookup,
    cache: BTreeMap<String, ComponentFacts>,
    declared: BTreeMap<String, Vec<ProjectFact>>,
    detections: u64,
}

impl std::fmt::Debug for ProjectFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectFacts")
            .field("root", &self.root)
            .field("cached", &self.cache.len())
            .field("detections", &self.detections)
            .finish()
    }
}

impl ProjectFacts {
    /// Facts for the workspace at `root`, looking tools up on `PATH`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_tool_lookup(root, Box::new(crate::quality::program_on_path))
    }

    /// Facts for the workspace at `root`, deciding tool presence with
    /// `tool_lookup`, so a caller need not depend on the host's `PATH`.
    #[must_use]
    pub fn with_tool_lookup(root: impl Into<PathBuf>, tool_lookup: ToolLookup) -> Self {
        Self {
            root: root.into(),
            tool_lookup,
            cache: BTreeMap::new(),
            declared: BTreeMap::new(),
            detections: 0,
        }
    }

    /// The workspace root these facts are about.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The component `path` belongs to: the nearest directory at or above it,
    /// within the workspace, that holds a project marker, or `.` when none
    /// does. A path inside a dependency or build directory belongs to the
    /// component above that directory. `None` for a path outside the workspace.
    #[must_use]
    pub fn component_of(&self, path: &Path) -> Option<String> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let rel = relative(&self.root, &absolute)?;
        let mut parts: Vec<&str> = if rel == "." {
            Vec::new()
        } else {
            rel.split('/').collect()
        };
        // A file (or a name with an extension that does not exist yet) is not
        // a directory: start at its parent.
        if !parts.is_empty() && !absolute.is_dir() {
            parts.pop();
        }
        // Everything from the outermost ignored directory down is left out.
        if let Some(cut) = parts.iter().position(|part| is_ignored(part)) {
            parts.truncate(cut);
        }
        while !parts.is_empty() {
            let dir = self.root.join(parts.join("/"));
            if has_marker(&dir) {
                return Some(parts.join("/"));
            }
            parts.pop();
        }
        Some(".".to_string())
    }

    /// The facts of `component` (as [`ProjectFacts::component_of`] names it),
    /// detected again only when a file or directory they were read from has
    /// changed, plus anything declared for it.
    pub fn facts_for(&mut self, component: &str) -> ComponentFacts {
        let current = self
            .cache
            .get(component)
            .is_some_and(|facts| facts.stamps.iter().all(Stamp::is_current));
        if !current {
            let detected = detect(&self.root, component, &self.tool_lookup);
            self.detections += 1;
            self.cache.insert(component.to_string(), detected);
        }
        let mut facts = self
            .cache
            .get(component)
            .cloned()
            .unwrap_or_else(|| ComponentFacts {
                component: component.to_string(),
                facts: Vec::new(),
                unknown: Vec::new(),
                stamps: Vec::new(),
            });
        if let Some(declared) = self.declared.get(component) {
            facts.facts.extend(declared.iter().cloned());
            sort_facts(&mut facts.facts);
        }
        facts
    }

    /// The facts of the component `path` belongs to; `None` outside the
    /// workspace.
    pub fn facts_for_path(&mut self, path: &Path) -> Option<ComponentFacts> {
        let component = self.component_of(path)?;
        Some(self.facts_for(&component))
    }

    /// Record a fact the user declared for `component`, such as the stack of
    /// a project that has no manifest yet. It is kept apart from detection,
    /// never replaces a confirmed fact, and survives a refresh.
    pub fn declare(&mut self, component: &str, id: impl Into<String>, value: Option<String>) {
        self.declared
            .entry(component.to_string())
            .or_default()
            .push(ProjectFact {
                id: id.into(),
                value: value.map(|value| bounded(&value)),
                confidence: FactConfidence::Declared,
                source: None,
            });
    }

    /// Forget every cached detection, so the next request reads again.
    /// Declared facts are kept.
    pub fn refresh(&mut self) {
        self.cache.clear();
    }

    /// How many detections have run. A diagnostic: a request answered from the
    /// cache does not add one.
    #[must_use]
    pub fn detections(&self) -> u64 {
        self.detections
    }

    /// Every component directory within two levels of the root, the root
    /// first. A fallback inventory: a task's own component comes from its
    /// paths, not from this list.
    #[must_use]
    pub fn components(&self) -> Vec<String> {
        let mut found = vec![".".to_string()];
        let mut frontier = vec![(self.root.clone(), 0usize)];
        let mut listed = 0usize;
        while let Some((dir, depth)) = frontier.pop() {
            if depth >= INVENTORY_DEPTH {
                continue;
            }
            let mut children: Vec<PathBuf> = list_dir(&dir)
                .into_iter()
                .filter(|path| path.is_dir())
                .filter(|path| {
                    file_name(path).is_some_and(|name| !name.starts_with('.') && !is_ignored(name))
                })
                .collect();
            children.sort();
            for child in children {
                listed += 1;
                if listed > MAX_INVENTORY_DIRS {
                    return finish_inventory(found);
                }
                if has_marker(&child) {
                    if let Some(rel) = relative(&self.root, &child) {
                        found.push(rel);
                    }
                }
                frontier.push((child, depth + 1));
            }
        }
        finish_inventory(found)
    }
}

fn finish_inventory(mut found: Vec<String>) -> Vec<String> {
    found[1..].sort();
    found
}

/// What one detection is collecting.
struct Detection<'a> {
    root: &'a Path,
    facts: Vec<ProjectFact>,
    unknown: Vec<UnknownFact>,
    stamps: Vec<Stamp>,
}

impl Detection<'_> {
    fn fact(&mut self, id: &str, value: Option<&str>, confidence: FactConfidence, source: &Path) {
        self.facts.push(ProjectFact {
            id: id.to_string(),
            value: value.map(bounded),
            confidence,
            source: relative(self.root, source),
        });
    }

    fn confirmed(&mut self, id: &str, value: Option<&str>, source: &Path) {
        self.fact(id, value, FactConfidence::Confirmed, source);
    }

    fn tentative(&mut self, id: &str, value: Option<&str>, source: &Path) {
        self.fact(id, value, FactConfidence::Tentative, source);
    }

    fn unknown(&mut self, id: &str, reason: impl Into<String>) {
        self.unknown.push(UnknownFact {
            id: id.to_string(),
            reason: reason.into(),
        });
    }

    /// Record that facts depend on `path`, present or not.
    fn stamp(&mut self, path: &Path) {
        if self.stamps.iter().all(|stamp| stamp.path != path) {
            self.stamps.push(Stamp::take(self.root, path));
        }
    }

    /// Read `path` as text within `limit` bytes, stamping it either way.
    /// `None` when it is absent; an unknown under `id` when it cannot be read.
    fn read(&mut self, id: &str, path: &Path, limit: u64) -> Option<String> {
        self.stamp(path);
        let meta = std::fs::metadata(path).ok()?;
        let shown = relative(self.root, path).unwrap_or_default();
        if meta.len() > limit {
            self.unknown(
                id,
                format!("{shown} is larger than the {limit}-byte read limit"),
            );
            return None;
        }
        match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(err) => {
                self.unknown(id, format!("{shown} could not be read: {err}"));
                None
            }
        }
    }

    fn tool(&mut self, program: &str, lookup: &ToolLookup) {
        let id = format!("tool.{program}");
        if lookup(program) {
            self.facts.push(ProjectFact {
                id,
                value: None,
                confidence: FactConfidence::Confirmed,
                source: None,
            });
        } else {
            self.unknown(&id, format!("`{program}` was not found on PATH"));
        }
    }
}

/// Detect the facts of `component` under `root`.
fn detect(root: &Path, component: &str, lookup: &ToolLookup) -> ComponentFacts {
    let dir = if component == "." {
        root.to_path_buf()
    } else {
        root.join(component)
    };
    let mut detection = Detection {
        root,
        facts: Vec::new(),
        unknown: Vec::new(),
        stamps: Vec::new(),
    };
    // A new marker file changes the listing's directory stamp.
    detection.stamp(&dir);
    let entries = list_dir(&dir);
    let names: Vec<String> = entries
        .iter()
        .filter_map(|path| file_name(path).map(str::to_string))
        .collect();

    let engine = detect_engines(&mut detection, &dir, &names, lookup);
    let rust = detect_rust(&mut detection, root, &dir, lookup);
    let dotnet = detect_dotnet(&mut detection, root, &dir, &names, engine, lookup);
    let node = detect_node(&mut detection, &dir, lookup);
    let python = detect_python(&mut detection, &dir, lookup);
    let go = detect_go(&mut detection, &dir, lookup);

    if !(engine || rust || dotnet || node || python || go) {
        if names.iter().all(|name| name.starts_with('.')) {
            detection.confirmed("project.empty", None, &dir);
        } else {
            detection.unknown("stack", "no recognized project marker in this directory");
        }
    }

    let Detection {
        mut facts,
        unknown,
        stamps,
        ..
    } = detection;
    sort_facts(&mut facts);
    facts.dedup();
    ComponentFacts {
        component: component.to_string(),
        facts,
        unknown,
        stamps,
    }
}

/// Godot, Unity and Unreal. Returns whether an engine project was found.
fn detect_engines(
    detection: &mut Detection<'_>,
    dir: &Path,
    names: &[String],
    lookup: &ToolLookup,
) -> bool {
    let mut found = false;

    let godot = dir.join("project.godot");
    if let Some(text) = detection.read("godot.project", &godot, MAX_READ_BYTES) {
        found = true;
        detection.confirmed("godot.project", None, &godot);
        detection.confirmed("product.game", None, &godot);
        match godot_version(&text) {
            Some((version, true)) => detection.confirmed("godot.version", Some(&version), &godot),
            Some((version, false)) => detection.tentative("godot.version", Some(&version), &godot),
            None => detection.unknown("godot.version", "project.godot names no engine version"),
        }
        if let Some(scene) = ini_value(&text, "run/main_scene") {
            detection.confirmed("godot.main_scene", Some(&scene), &godot);
        }
        detection.tool("godot", lookup);
    }

    let unity = dir.join("ProjectSettings").join("ProjectVersion.txt");
    if let Some(text) = detection.read("unity.project", &unity, MAX_READ_BYTES) {
        found = true;
        detection.confirmed("unity.project", None, &unity);
        detection.confirmed("product.game", None, &unity);
        match text
            .lines()
            .find_map(|line| line.trim().strip_prefix("m_EditorVersion:"))
        {
            Some(version) => detection.confirmed("unity.version", Some(version.trim()), &unity),
            None => detection.unknown(
                "unity.version",
                "ProjectVersion.txt names no editor version",
            ),
        }
    }

    for name in names
        .iter()
        .filter(|name| has_extension(name, "uproject"))
        .take(MAX_PROJECT_FILES)
    {
        let path = dir.join(name);
        let Some(text) = detection.read("unreal.project", &path, MAX_READ_BYTES) else {
            continue;
        };
        found = true;
        detection.confirmed("unreal.project", Some(name), &path);
        detection.confirmed("product.game", None, &path);
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => {
                if let Some(version) = value.get("EngineAssociation").and_then(|v| v.as_str()) {
                    detection.confirmed("unreal.version", Some(version), &path);
                }
            }
            Err(err) => {
                detection.unknown("unreal.version", format!("{name} is not valid JSON: {err}"))
            }
        }
    }
    found
}

/// The engine version in `project.godot` and whether it was stated outright
/// (`config/features`) rather than inferred from the file format version.
fn godot_version(text: &str) -> Option<(String, bool)> {
    if let Some(features) = ini_value(text, "config/features") {
        let version = features
            .split('"')
            .skip(1)
            .step_by(2)
            .find(|item| item.chars().next().is_some_and(|c| c.is_ascii_digit()));
        if let Some(version) = version {
            return Some((version.to_string(), true));
        }
    }
    match ini_value(text, "config_version").as_deref() {
        Some("5") => Some(("4".to_string(), false)),
        Some("4") => Some(("3".to_string(), false)),
        _ => None,
    }
}

/// The value of `key=` in an INI-like file, without surrounding quotes.
fn ini_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().trim_matches('"').to_string())
    })
}

/// Cargo packages, workspaces and toolchain pins.
fn detect_rust(
    detection: &mut Detection<'_>,
    root: &Path,
    dir: &Path,
    lookup: &ToolLookup,
) -> bool {
    let manifest = dir.join("Cargo.toml");
    let Some(text) = detection.read("cargo.manifest", &manifest, MAX_READ_BYTES) else {
        return false;
    };
    let value = match text.parse::<toml::Table>() {
        Ok(value) => value,
        Err(err) => {
            detection.unknown(
                "cargo.manifest",
                format!(
                    "Cargo.toml is not valid TOML: {}",
                    first_line(&err.to_string())
                ),
            );
            // A manifest is there even if it cannot be read: still Rust.
            detection.confirmed("cargo.manifest", None, &manifest);
            detect_toolchain_pin(detection, root, dir);
            detection.tool("cargo", lookup);
            return true;
        }
    };

    if let Some(workspace) = value.get("workspace").and_then(toml::Value::as_table) {
        let members: Vec<&str> = workspace
            .get("members")
            .and_then(toml::Value::as_array)
            .map(|members| members.iter().filter_map(toml::Value::as_str).collect())
            .unwrap_or_default();
        let shown = members.join(", ");
        detection.confirmed(
            "cargo.workspace",
            (!shown.is_empty()).then_some(shown.as_str()),
            &manifest,
        );
    }

    if let Some(package) = value.get("package").and_then(toml::Value::as_table) {
        let name = package.get("name").and_then(toml::Value::as_str);
        detection.confirmed("cargo.package", name, &manifest);
        for (key, id) in [
            ("edition", "rust.edition"),
            ("rust-version", "rust.version_required"),
        ] {
            if let Some(found) = package.get(key).and_then(toml::Value::as_str) {
                detection.confirmed(id, Some(found), &manifest);
            }
        }
        let main = dir.join("src").join("main.rs");
        detection.stamp(&main);
        let bins = value
            .get("bin")
            .and_then(toml::Value::as_array)
            .map_or(0, Vec::len);
        if main.is_file() || bins > 0 {
            detection.confirmed("cargo.bin", None, &manifest);
            detection.tentative("product.cli", None, &manifest);
        }
        let lib = dir.join("src").join("lib.rs");
        detection.stamp(&lib);
        if lib.is_file() || value.contains_key("lib") {
            detection.confirmed("cargo.lib", None, &manifest);
            detection.tentative("product.library", None, &manifest);
        }
        let tests = dir.join("tests");
        detection.stamp(&tests);
        if tests.is_dir() || value.contains_key("test") {
            detection.confirmed("test.target", Some("tests"), &manifest);
        }
        if !value.contains_key("workspace") {
            detect_workspace_root(detection, root, dir);
        }
    }

    detect_toolchain_pin(detection, root, dir);
    detection.tool("cargo", lookup);
    true
}

/// The nearest ancestor `Cargo.toml` with a `[workspace]` table, within the
/// workspace root.
fn detect_workspace_root(detection: &mut Detection<'_>, root: &Path, dir: &Path) {
    for ancestor in ancestors_within(root, dir) {
        let manifest = ancestor.join("Cargo.toml");
        let Some(text) = detection.read("cargo.workspace_member", &manifest, MAX_READ_BYTES) else {
            continue;
        };
        if text
            .parse::<toml::Table>()
            .is_ok_and(|value| value.contains_key("workspace"))
        {
            let at = relative(root, &ancestor).unwrap_or_else(|| ".".to_string());
            detection.confirmed("cargo.workspace_member", Some(&at), &manifest);
            return;
        }
    }
}

/// A `rust-toolchain.toml` or `rust-toolchain` file in the component or an
/// ancestor within the workspace.
fn detect_toolchain_pin(detection: &mut Detection<'_>, root: &Path, dir: &Path) {
    for at in std::iter::once(dir.to_path_buf()).chain(ancestors_within(root, dir)) {
        let toml_pin = at.join("rust-toolchain.toml");
        if let Some(text) = detection.read("rust.toolchain", &toml_pin, MAX_READ_BYTES) {
            match text.parse::<toml::Table>().ok().and_then(|value| {
                value
                    .get("toolchain")?
                    .get("channel")?
                    .as_str()
                    .map(str::to_string)
            }) {
                Some(channel) => detection.confirmed("rust.toolchain", Some(&channel), &toml_pin),
                None => detection.unknown("rust.toolchain", "rust-toolchain.toml names no channel"),
            }
            return;
        }
        let plain_pin = at.join("rust-toolchain");
        if let Some(text) = detection.read("rust.toolchain", &plain_pin, MAX_READ_BYTES) {
            let channel = first_line(&text).trim().to_string();
            if !channel.is_empty() {
                detection.confirmed("rust.toolchain", Some(&channel), &plain_pin);
            }
            return;
        }
    }
}

/// SDK-style .NET projects, solutions and SDK pins. `engine` marks project
/// files that sit beside an engine project as generated.
fn detect_dotnet(
    detection: &mut Detection<'_>,
    root: &Path,
    dir: &Path,
    names: &[String],
    engine: bool,
    lookup: &ToolLookup,
) -> bool {
    let mut found = false;
    for name in names
        .iter()
        .filter(|name| has_extension(name, "sln") || has_extension(name, "slnx"))
        .take(MAX_PROJECT_FILES)
    {
        found = true;
        detection.confirmed("dotnet.solution", Some(name), &dir.join(name));
    }
    for name in names
        .iter()
        .filter(|name| has_extension(name, "csproj") || has_extension(name, "fsproj"))
        .take(MAX_PROJECT_FILES)
    {
        let path = dir.join(name);
        let Some(text) = detection.read("dotnet.project", &path, MAX_READ_BYTES) else {
            continue;
        };
        found = true;
        if engine {
            detection.confirmed("generated.csproj", Some(name), &path);
        }
        let Some(sdk) = xml_attribute(&text, "Project", "Sdk") else {
            detection.unknown(
                "dotnet.sdk_project",
                format!("{name} is not an SDK-style project; that format is not supported"),
            );
            continue;
        };
        detection.confirmed("dotnet.sdk_project", Some(name), &path);
        match sdk.as_str() {
            "Microsoft.NET.Sdk.Web" => {
                detection.confirmed("aspnet.project", Some(name), &path);
                detection.tentative("product.service", None, &path);
            }
            sdk if sdk.starts_with("Godot.NET.Sdk") => {
                detection.confirmed("godot.csharp", Some(sdk), &path);
            }
            _ => {}
        }
        if xml_element(&text, "UseWPF").is_some_and(|value| value.eq_ignore_ascii_case("true")) {
            detection.confirmed("wpf.project", Some(name), &path);
            detection.confirmed("product.desktop", None, &path);
        }
        if package_reference(&text, "Avalonia") {
            detection.confirmed("avalonia.project", Some(name), &path);
            detection.confirmed("product.desktop", None, &path);
        }
        if [
            "Microsoft.NET.Test.Sdk",
            "xunit",
            "NUnit",
            "MSTest.TestFramework",
            "MSTest",
        ]
        .iter()
        .any(|package| package_reference(&text, package))
        {
            detection.confirmed("dotnet.test_project", Some(name), &path);
            detection.confirmed("test.target", Some(name), &path);
        }
        for element in ["TargetFramework", "TargetFrameworks"] {
            if let Some(framework) = xml_element(&text, element) {
                detection.confirmed("dotnet.target_framework", Some(&framework), &path);
            }
        }
    }
    if found {
        for at in std::iter::once(dir.to_path_buf()).chain(ancestors_within(root, dir)) {
            let pin = at.join("global.json");
            if let Some(text) = detection.read("dotnet.sdk", &pin, MAX_READ_BYTES) {
                match serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("sdk")?
                            .get("version")?
                            .as_str()
                            .map(str::to_string)
                    }) {
                    Some(version) => detection.confirmed("dotnet.sdk", Some(&version), &pin),
                    None => detection.unknown("dotnet.sdk", "global.json names no SDK version"),
                }
                break;
            }
        }
        detection.tool("dotnet", lookup);
    }
    found
}

/// A Node package: watched dependencies with their declared range and, from
/// `package-lock.json`, the installed version; watched scripts.
fn detect_node(detection: &mut Detection<'_>, dir: &Path, lookup: &ToolLookup) -> bool {
    let manifest = dir.join("package.json");
    let Some(text) = detection.read("node.package", &manifest, MAX_READ_BYTES) else {
        return false;
    };
    let value = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => value,
        Err(err) => {
            detection.confirmed("node.package", None, &manifest);
            detection.unknown(
                "node.package",
                format!("package.json is not valid JSON: {err}"),
            );
            detection.tool("node", lookup);
            return true;
        }
    };
    detection.confirmed(
        "node.package",
        value.get("name").and_then(|name| name.as_str()),
        &manifest,
    );

    let mut declared: BTreeMap<&str, String> = BTreeMap::new();
    for key in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        let Some(deps) = value.get(key).and_then(|deps| deps.as_object()) else {
            continue;
        };
        for name in WATCHED_NODE_DEPENDENCIES {
            if let Some(range) = deps.get(*name).and_then(|range| range.as_str()) {
                declared.entry(name).or_insert_with(|| range.to_string());
            }
        }
    }
    for (name, range) in &declared {
        detection.confirmed(&format!("dependency.{name}"), Some(range), &manifest);
    }
    if !declared.is_empty() {
        let lock = dir.join("package-lock.json");
        if let Some(text) = detection.read("version.from_lockfile", &lock, MAX_LOCKFILE_BYTES) {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(lockfile) => {
                    for name in declared.keys() {
                        let installed = lockfile
                            .get("packages")
                            .and_then(|packages| packages.get(format!("node_modules/{name}")))
                            .or_else(|| {
                                lockfile
                                    .get("dependencies")
                                    .and_then(|deps| deps.get(*name))
                            })
                            .and_then(|entry| entry.get("version"))
                            .and_then(|version| version.as_str());
                        if let Some(version) = installed {
                            detection.confirmed(
                                &format!("dependency.{name}.locked"),
                                Some(version),
                                &lock,
                            );
                        }
                    }
                }
                Err(err) => detection.unknown(
                    "version.from_lockfile",
                    format!("package-lock.json is not valid JSON: {err}"),
                ),
            }
        }
    }

    if let Some(scripts) = value.get("scripts").and_then(|scripts| scripts.as_object()) {
        for name in WATCHED_SCRIPTS {
            if let Some(command) = scripts.get(*name).and_then(|command| command.as_str()) {
                let command = localpilot_config::redact::redact(command);
                detection.confirmed(&format!("script.{name}"), Some(&command), &manifest);
                if *name == "test" {
                    detection.confirmed("test.target", Some("script.test"), &manifest);
                }
            }
        }
    }

    let index = dir.join("index.html");
    detection.stamp(&index);
    if declared.contains_key("electron") {
        detection.tentative("product.desktop", None, &manifest);
    } else if index.is_file() || declared.contains_key("vite") || declared.contains_key("next") {
        detection.tentative("product.browser", None, &manifest);
    }
    detection.tool("node", lookup);
    true
}

/// A Python project from `pyproject.toml`, or tentatively from
/// `requirements.txt`.
fn detect_python(detection: &mut Detection<'_>, dir: &Path, lookup: &ToolLookup) -> bool {
    let manifest = dir.join("pyproject.toml");
    let tests = dir.join("tests");
    detection.stamp(&tests);
    if let Some(text) = detection.read("python.project", &manifest, MAX_READ_BYTES) {
        match text.parse::<toml::Table>() {
            Ok(value) => {
                let project = value.get("project").and_then(toml::Value::as_table);
                detection.confirmed(
                    "python.project",
                    project
                        .and_then(|p| p.get("name"))
                        .and_then(toml::Value::as_str),
                    &manifest,
                );
                if let Some(required) = project
                    .and_then(|p| p.get("requires-python"))
                    .and_then(toml::Value::as_str)
                {
                    detection.confirmed("python.version_required", Some(required), &manifest);
                }
                let pytest = value
                    .get("tool")
                    .and_then(|tool| tool.get("pytest"))
                    .is_some();
                if pytest || tests.is_dir() {
                    detection.confirmed("test.target", Some("tests"), &manifest);
                }
            }
            Err(err) => {
                detection.confirmed("python.project", None, &manifest);
                detection.unknown(
                    "python.project",
                    format!(
                        "pyproject.toml is not valid TOML: {}",
                        first_line(&err.to_string())
                    ),
                );
            }
        }
        detection.tool(if cfg!(windows) { "python" } else { "python3" }, lookup);
        return true;
    }
    let requirements = dir.join("requirements.txt");
    detection.stamp(&requirements);
    if requirements.is_file() {
        detection.tentative("python.project", None, &requirements);
        detection.tool(if cfg!(windows) { "python" } else { "python3" }, lookup);
        return true;
    }
    false
}

/// A Go module.
fn detect_go(detection: &mut Detection<'_>, dir: &Path, lookup: &ToolLookup) -> bool {
    let manifest = dir.join("go.mod");
    let Some(text) = detection.read("go.module", &manifest, MAX_READ_BYTES) else {
        return false;
    };
    let module = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("module "))
        .map(str::trim);
    detection.confirmed("go.module", module, &manifest);
    if let Some(version) = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("go "))
        .map(str::trim)
    {
        detection.confirmed("go.version", Some(version), &manifest);
    }
    detection.tool("go", lookup);
    true
}

/// Whether `dir` holds a project marker.
fn has_marker(dir: &Path) -> bool {
    const FILES: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "project.godot",
    ];
    if FILES.iter().any(|name| dir.join(name).is_file())
        || dir
            .join("ProjectSettings")
            .join("ProjectVersion.txt")
            .is_file()
    {
        return true;
    }
    list_dir(dir).iter().any(|path| {
        file_name(path).is_some_and(|name| {
            ["csproj", "fsproj", "uproject"]
                .iter()
                .any(|ext| has_extension(name, ext))
        }) && path.is_file()
    })
}

/// The entries of `dir`, at most [`MAX_DIR_ENTRIES`], sorted; empty when it
/// cannot be listed.
fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .take(MAX_DIR_ENTRIES)
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths
}

/// The ancestors of `dir` strictly above it, up to and including `root`.
fn ancestors_within(root: &Path, dir: &Path) -> Vec<PathBuf> {
    if dir == root || !dir.starts_with(root) {
        return Vec::new();
    }
    dir.ancestors()
        .skip(1)
        .take_while(|ancestor| ancestor.starts_with(root))
        .map(Path::to_path_buf)
        .collect()
}

/// `path` relative to `root` with `/` separators, `.` for the root itself;
/// `None` outside it. `..` components are resolved lexically.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    let rest = normalized.strip_prefix(root).ok()?;
    let text = rest.to_string_lossy().replace('\\', "/");
    Some(if text.is_empty() {
        ".".to_string()
    } else {
        text
    })
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
}

fn is_ignored(name: &str) -> bool {
    name.starts_with('.') || IGNORED_DIRS.contains(&name)
}

fn has_extension(name: &str, ext: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|found| found.to_str())
        .is_some_and(|found| found.eq_ignore_ascii_case(ext))
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn bounded(value: &str) -> String {
    if value.chars().count() <= MAX_VALUE_CHARS {
        return value.to_string();
    }
    let mut out: String = value.chars().take(MAX_VALUE_CHARS).collect();
    out.push('…');
    out
}

fn sort_facts(facts: &mut [ProjectFact]) {
    facts.sort_by(|a, b| {
        (a.id.as_str(), a.value.as_deref(), a.confidence).cmp(&(
            b.id.as_str(),
            b.value.as_deref(),
            b.confidence,
        ))
    });
}

/// The value of attribute `name` on the first `<element ...>` tag.
fn xml_attribute(text: &str, element: &str, name: &str) -> Option<String> {
    let start = text.find(&format!("<{element}"))?;
    let tag_end = text[start..].find('>')? + start;
    let tag = &text[start..tag_end];
    let at = tag.find(&format!("{name}="))? + name.len() + 1;
    let quote = tag[at..].chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value_start = at + 1;
    let value_end = tag[value_start..].find(quote)? + value_start;
    Some(tag[value_start..value_end].trim().to_string())
}

/// The text of the first `<element>...</element>`.
fn xml_element(text: &str, element: &str) -> Option<String> {
    let open = format!("<{element}>");
    let close = format!("</{element}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    let value = text[start..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Whether the project references package `name` (exactly, case-insensitive).
fn package_reference(text: &str, name: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let wanted = name.to_ascii_lowercase();
    lower.match_indices("<packagereference").any(|(at, _)| {
        let tag = &lower[at..lower[at..].find('>').map_or(lower.len(), |end| at + end)];
        tag.contains(&format!("include=\"{wanted}\""))
            || tag.contains(&format!("include='{wanted}'"))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_godot_version_is_stated_or_inferred_from_the_format() {
        let stated = "config_version=5\n[application]\nconfig/features=PackedStringArray(\"4.2\", \"Forward Plus\")\n";
        assert_eq!(godot_version(stated), Some(("4.2".to_string(), true)));
        assert_eq!(
            godot_version("config_version=5\n"),
            Some(("4".to_string(), false))
        );
        assert_eq!(
            godot_version("config_version=4\n"),
            Some(("3".to_string(), false))
        );
        assert_eq!(godot_version("[application]\n"), None);
    }

    #[test]
    fn project_xml_is_read_without_a_parser() {
        let text = r#"<Project Sdk="Microsoft.NET.Sdk.Web">
  <PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup>
  <ItemGroup><PackageReference Include="xunit" Version="2.9.0" /><PackageReference Include="Avalonia.Themes" /></ItemGroup>
</Project>"#;
        assert_eq!(
            xml_attribute(text, "Project", "Sdk").as_deref(),
            Some("Microsoft.NET.Sdk.Web")
        );
        assert_eq!(
            xml_element(text, "TargetFramework").as_deref(),
            Some("net8.0")
        );
        assert!(package_reference(text, "xunit"));
        // A package whose name only starts with the one asked for is not it.
        assert!(!package_reference(text, "Avalonia"));
    }

    #[test]
    fn a_relative_path_never_escapes_the_root() {
        let root = Path::new("/w");
        assert_eq!(relative(root, Path::new("/w")).as_deref(), Some("."));
        assert_eq!(relative(root, Path::new("/w/a/b")).as_deref(), Some("a/b"));
        assert_eq!(relative(root, Path::new("/w/a/../b")).as_deref(), Some("b"));
        assert_eq!(relative(root, Path::new("/w/../x")), None);
        assert_eq!(relative(root, Path::new("/x")), None);
    }

    #[test]
    fn long_values_are_bounded() {
        let long = "x".repeat(MAX_VALUE_CHARS + 10);
        assert_eq!(bounded(&long).chars().count(), MAX_VALUE_CHARS + 1);
        assert_eq!(bounded("short"), "short");
    }
}
