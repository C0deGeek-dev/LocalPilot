//! The optional `[guidance]` section, read through ordinary skill discovery.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use localpilot_skills::{
    discovery_roots, Applicability, BodySection, GuidanceRole, SkillScope, SkillSet,
};

fn package(root: &Path, name: &str, manifest_extra: &str, body: &str) {
    let dir = root.join(".localpilot").join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("skill.toml"),
        format!(
            "name = \"{name}\"\ndescription = \"Fix a failing command line exit code\"\nversion = \"1.2.0\"\n\
             [triggers]\ncommands = [\"exitfix\"]\n{manifest_extra}"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
}

fn resolve(root: &Path) -> SkillSet {
    SkillSet::resolve(&discovery_roots(root, None, true)).unwrap()
}

#[test]
fn a_valid_section_is_read_beside_the_unchanged_manifest() {
    let dir = tempfile::tempdir().unwrap();
    package(
        dir.path(),
        "rust-cli",
        "[guidance]\nschema_version = 1\nrole = \"supplement\"\nstacks = [\"rust\"]\n\
         products = [\"cli\"]\nbody_sections = [\"#Exit codes\", \"references/cargo.md\"]\n",
        "Intro.\n\n## Exit codes\nCheck stderr too.\n",
    );
    let refs = dir.path().join(".localpilot/skills/rust-cli/references");
    std::fs::create_dir_all(&refs).unwrap();
    std::fs::write(refs.join("cargo.md"), "Cargo notes.\n").unwrap();

    let set = resolve(dir.path());
    let skill = set.by_name("rust-cli").unwrap();
    let metadata = skill.guidance.metadata().unwrap();
    assert_eq!(metadata.role, GuidanceRole::Supplement);
    assert_eq!(metadata.products, ["cli"]);
    assert_eq!(
        metadata.body_sections,
        [
            BodySection::Heading("Exit codes".into()),
            BodySection::File("references/cargo.md".into())
        ]
    );
    assert_eq!(skill.manifest.version, "1.2.0");
    assert_eq!(skill.manifest.triggers.commands, ["exitfix"]);
}

#[test]
fn an_unusable_section_keeps_the_package_listed_searchable_and_loadable() {
    let with_bad = tempfile::tempdir().unwrap();
    package(
        with_bad.path(),
        "exit-fix",
        "[guidance]\nschema_version = 9\nintents = [\"investigate\"]\npermissions = [\"net:*\"]\n",
        "Body.\n",
    );
    let without = tempfile::tempdir().unwrap();
    package(without.path(), "exit-fix", "", "Body.\n");

    let bad = resolve(with_bad.path());
    let plain = resolve(without.path());

    assert!(bad.skipped().is_empty(), "{:?}", bad.skipped());
    let skill = bad.by_name("exit-fix").unwrap();
    assert!(matches!(skill.guidance, Applicability::Invalid(_)));
    assert_eq!(bad.discoverable_count(), 1);
    // Ranking is the same as for the same package without the section.
    let rank = |set: &SkillSet| -> Vec<(String, u32)> {
        set.ranked("exit code")
            .into_iter()
            .map(|(skill, score)| (skill.manifest.name.clone(), score))
            .collect()
    };
    assert_eq!(rank(&bad), rank(&plain));
    assert!(!rank(&bad).is_empty());
    assert_eq!(
        skill.instructions,
        plain.by_name("exit-fix").unwrap().instructions
    );
    assert!(skill.manifest.permissions.is_empty());
    assert_eq!(
        plain.by_name("exit-fix").unwrap().guidance,
        Applicability::Absent
    );
}

#[test]
fn a_skill_md_only_package_has_no_section() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".agents/skills/plain");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: plain\ndescription: A plain package\n---\nBody.\n",
    )
    .unwrap();

    let set = resolve(dir.path());
    let skill = set.by_name("plain").unwrap();
    assert_eq!(skill.scope, SkillScope::ProjectAgents);
    assert_eq!(skill.guidance, Applicability::Absent);
}

#[cfg(unix)]
#[test]
fn a_section_file_that_links_outside_the_package_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret.md"), "outside\n").unwrap();
    package(
        dir.path(),
        "linked",
        "[guidance]\nschema_version = 1\nintents = [\"review\"]\nbody_sections = [\"notes.md\"]\n",
        "Body.\n",
    );
    std::os::unix::fs::symlink(
        dir.path().join("secret.md"),
        dir.path().join(".localpilot/skills/linked/notes.md"),
    )
    .unwrap();
    let set = resolve(dir.path());
    assert!(matches!(
        set.by_name("linked").unwrap().guidance,
        Applicability::Invalid(_)
    ));
}
