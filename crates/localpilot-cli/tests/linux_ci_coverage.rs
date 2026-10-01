//! Every workspace package must execute its tests in the Linux gating job.

use std::{collections::BTreeSet, path::Path, process::Command};

use anyhow::Context as _;
use serde::Deserialize;

#[derive(Deserialize)]
struct Workflow {
    jobs: std::collections::BTreeMap<String, Job>,
}

#[derive(Deserialize)]
struct Job {
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    #[serde(rename = "if", default)]
    condition: String,
    #[serde(default)]
    run: String,
}

fn linux_test_packages(workflow: &str) -> anyhow::Result<BTreeSet<String>> {
    let workflow: Workflow = serde_yaml::from_str(workflow)?;
    let job = workflow
        .jobs
        .get("test")
        .context("the gating test job is missing")?;
    let mut packages = BTreeSet::new();
    for step in &job.steps {
        if step.condition != "runner.os == 'Linux'" {
            continue;
        }
        for line in step
            .run
            .lines()
            .filter(|line| !line.trim().starts_with('#'))
        {
            let words: Vec<_> = line.split_whitespace().collect();
            let Some(start) = words
                .windows(3)
                .position(|part| part == ["cargo", "nextest", "run"])
            else {
                continue;
            };
            let args = &words[start + 3..];
            if args.contains(&"--no-run") {
                continue;
            }
            for pair in args.windows(2) {
                if pair[0] == "-p" {
                    packages.insert(pair[1].to_owned());
                }
            }
        }
    }
    Ok(packages)
}

#[derive(Deserialize)]
struct Metadata {
    workspace_members: BTreeSet<String>,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
}

#[test]
fn every_workspace_package_runs_tests_on_linux() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .current_dir(&root)
        .output()
        .expect("cargo metadata should start");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Metadata = serde_json::from_slice(&output.stdout).expect("Cargo metadata JSON");
    let expected: BTreeSet<_> = metadata
        .packages
        .into_iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .map(|package| package.name)
        .collect();
    let workflow = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
        .expect("the gating workflow exists");
    let actual = linux_test_packages(&workflow).expect("valid gating workflow");
    let missing: Vec<_> = expected.difference(&actual).collect();
    let unknown: Vec<_> = actual.difference(&expected).collect();
    assert!(
        missing.is_empty() && unknown.is_empty(),
        "Linux gating test steps must match workspace packages; missing: {missing:?}; unknown: {unknown:?}"
    );
}

#[test]
fn coverage_builds_comments_and_other_platforms_do_not_count_as_linux_tests() {
    let workflow = r#"
jobs:
  test:
    steps:
      - if: runner.os != 'Linux'
        run: cargo nextest run -p windows-only
      - if: runner.os == 'Linux'
        run: cargo nextest run -p build-only --no-run
      - if: runner.os == 'Linux'
        run: |
          # cargo nextest run -p commented-out
          capped cargo nextest run -p tested --test-threads 2
      - if: runner.os == 'Linux'
        run: |
          timeout -k 30s 15m capped cargo nextest run -p corpus --test-threads 1 > log || status=$?
          exit "$status"
  coverage:
    steps:
      - if: runner.os == 'Linux'
        run: cargo nextest run -p coverage-only
"#;
    assert_eq!(
        linux_test_packages(workflow).expect("valid fixture"),
        BTreeSet::from(["tested".to_owned(), "corpus".to_owned()])
    );
}
