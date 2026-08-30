//! Exactly what would leave the machine for a revalidation pass. Sends nothing.
//!
//! The set of names treated as public comes from what this workspace declares as
//! third-party dependencies — public by construction, because they were fetched
//! from a public registry. Workspace-internal crates (path dependencies) are not
//! in it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]
use localmind_store::{derive_verification_query, is_revalidation_candidate, MemoryPersistence};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Third-party dependency names declared anywhere under `root`.
fn declared_dependencies(root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skip = path
                    .file_name()
                    .is_some_and(|n| matches!(n.to_string_lossy().as_ref(), "target" | ".git"));
                if !skip {
                    stack.push(path);
                }
                continue;
            }
            if path.file_name().is_some_and(|n| n == "Cargo.toml") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let mut section = String::new();
                for line in text.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with('[') {
                        section = trimmed.trim_matches(['[', ']']).to_string();
                        continue;
                    }
                    if !section.contains("dependencies") {
                        continue;
                    }
                    let Some((name, rhs)) = trimmed.split_once('=') else {
                        continue;
                    };
                    // A path or workspace dependency is one of ours, not public.
                    if rhs.contains("path") || rhs.contains("workspace") {
                        continue;
                    }
                    let name = name.trim();
                    if !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    {
                        names.insert(name.to_ascii_lowercase().replace('-', "_"));
                    }
                }
            }
        }
    }
    names
}

fn main() {
    let root = std::env::args().nth(1).map(PathBuf::from).expect("root");
    let public = declared_dependencies(&root);
    eprintln!(
        "public names (declared third-party dependencies): {}",
        public.len()
    );
    let store = MemoryPersistence::open_project(&root).expect("store");
    let records = store.list_memory().expect("memories");
    let (mut candidates, mut sends) = (0, 0);
    for record in &records {
        if !is_revalidation_candidate(&record.body) {
            continue;
        }
        candidates += 1;
        if let Some(query) = derive_verification_query(&record.body, &public) {
            sends += 1;
            println!("| `{}` | `{}` |", record.memory_id, query);
        }
    }
    println!(
        "\ncandidates {candidates} of {}   would send {sends}   would send nothing {}",
        records.len(),
        candidates - sends
    );
}
