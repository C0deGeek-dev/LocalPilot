//! Prove the guidance instrument before trusting it.
//!
//! An authored fixture corpus of ideas — each hand-labelled with its true
//! decision axes and which of them the idea resolves — drives three checks:
//!
//! - **Offline ordering self-test** (the CI gate): scored from scripted
//!   responses, every under-specified fixture must score strictly below every
//!   well-specified fixture. If it cannot, the instrument — not the threshold
//!   — is broken, and the failure names the offending pair rather than
//!   letting a meaningless number gate anything.
//! - **Corpus consistency**: the scripted responses themselves must satisfy
//!   the ground-truth recall/precision checks, so fixture drift is caught the
//!   same way model drift would be.
//! - **Live recall/precision drift checks** (opt-in via
//!   `LOCALPILOT_LIVE_TESTS`, never default CI): the same corpus against a
//!   real provider. Recall — every ground-truth axis must be semantically
//!   matched by some model axis; a miss fails the fixture regardless of the
//!   score, because a silently dropped axis inflates the score invisibly.
//!   Precision — matched axes must carry the right resolved/not-specified
//!   label.
//!
//! Every idea and every scripted reply is authored for this repository.
#![allow(clippy::unwrap_used)]

use localpilot_harness::{assess_guidance, DecisionAxis, GuidanceAssessment};
use localpilot_llm::FakeProvider;

/// One hand-labelled ground-truth decision axis: a human label, the lowercase
/// terms that count as a semantic match against a model-proposed axis (name or
/// question), and whether the idea's own words resolve it.
struct TruthAxis {
    label: &'static str,
    terms: &'static [&'static str],
    resolved: bool,
}

/// One authored fixture idea with its ground truth and a scripted model reply
/// used by the offline legs.
struct Fixture {
    name: &'static str,
    idea: &'static str,
    well_specified: bool,
    truth: &'static [TruthAxis],
    scripted_reply: &'static str,
}

const CORPUS: &[Fixture] = &[
    // --- well-specified: every load-bearing decision is in the idea text ---
    Fixture {
        name: "cli-todo",
        idea: "Build a command-line todo app in Rust for Linux, macOS, and Windows \
               terminals. Tasks live in a single JSON file at ~/.todo.json. Commands: \
               add, list, done, rm. No due dates, no sync, no colors.",
        well_specified: true,
        truth: &[
            TruthAxis {
                label: "interface",
                terms: &["command-line", "cli", "interface", "command"],
                resolved: true,
            },
            TruthAxis {
                label: "storage",
                terms: &["storage", "json", "file", "persist"],
                resolved: true,
            },
            TruthAxis {
                label: "platform",
                terms: &["platform", "os", "linux", "windows"],
                resolved: true,
            },
            TruthAxis {
                label: "feature scope",
                terms: &["scope", "feature", "command set"],
                resolved: true,
            },
        ],
        scripted_reply: r#"{"axes":[
            {"axis":"interface","resolved":true,"evidence":"command-line todo app","question":""},
            {"axis":"storage","resolved":true,"evidence":"single JSON file at ~/.todo.json","question":""},
            {"axis":"platform","resolved":true,"evidence":"Linux, macOS, and Windows terminals","question":""},
            {"axis":"feature scope","resolved":true,"evidence":"add, list, done, rm. No due dates, no sync, no colors","question":""}
        ]}"#,
    },
    Fixture {
        name: "rate-limiter",
        idea: "Add a token-bucket rate limiter middleware to our existing Axum API: \
               100 requests per minute per API key, state kept in process memory, \
               and over-limit requests get 429 with a Retry-After header. No Redis, \
               no per-route overrides.",
        well_specified: true,
        truth: &[
            TruthAxis {
                label: "algorithm",
                terms: &["algorithm", "token-bucket", "token bucket", "strategy"],
                resolved: true,
            },
            TruthAxis {
                label: "limit policy",
                terms: &["limit", "quota", "per api key", "rate"],
                resolved: true,
            },
            TruthAxis {
                label: "state storage",
                terms: &["storage", "state", "memory", "redis"],
                resolved: true,
            },
            TruthAxis {
                label: "over-limit behaviour",
                terms: &[
                    "429",
                    "response",
                    "over-limit",
                    "reject",
                    "behaviour",
                    "behavior",
                ],
                resolved: true,
            },
        ],
        scripted_reply: r#"{"axes":[
            {"axis":"limiting algorithm","resolved":true,"evidence":"token-bucket rate limiter","question":""},
            {"axis":"limit policy","resolved":true,"evidence":"100 requests per minute per API key","question":""},
            {"axis":"state storage","resolved":true,"evidence":"state kept in process memory","question":""},
            {"axis":"over-limit behaviour","resolved":true,"evidence":"429 with a Retry-After header","question":""},
            {"axis":"route coverage","resolved":false,"evidence":"not specified","question":"Does the limit apply to every route, including health checks?"}
        ]}"#,
    },
    Fixture {
        name: "md2html",
        idea: "Write a single-binary Markdown-to-HTML converter: it takes one .md \
               file path as its only argument, writes a sibling .html file, supports \
               CommonMark only with no extensions, emits no CSS, and exits non-zero \
               on parse errors.",
        well_specified: true,
        truth: &[
            TruthAxis {
                label: "input/output contract",
                terms: &["input", "output", "argument", "sibling", "file"],
                resolved: true,
            },
            TruthAxis {
                label: "dialect",
                terms: &["dialect", "commonmark", "extension", "flavor", "flavour"],
                resolved: true,
            },
            TruthAxis {
                label: "styling",
                terms: &["css", "style", "styling", "theme"],
                resolved: true,
            },
            TruthAxis {
                label: "error behaviour",
                terms: &["error", "exit", "parse failure"],
                resolved: true,
            },
        ],
        scripted_reply: r#"{"axes":[
            {"axis":"input/output contract","resolved":true,"evidence":"one .md file path as its only argument, writes a sibling .html file","question":""},
            {"axis":"markdown dialect","resolved":true,"evidence":"CommonMark only with no extensions","question":""},
            {"axis":"styling","resolved":true,"evidence":"emits no CSS","question":""},
            {"axis":"error behaviour","resolved":true,"evidence":"exits non-zero on parse errors","question":""}
        ]}"#,
    },
    Fixture {
        name: "log-rotate",
        idea: "A cron-friendly log rotation tool: it rotates any file over 100 MB \
               under the directories listed in /etc/logrot.toml, keeps five gzip \
               generations per file, and runs once per invocation — no daemon mode.",
        well_specified: true,
        truth: &[
            TruthAxis {
                label: "rotation trigger",
                terms: &["trigger", "100 mb", "size", "threshold", "when"],
                resolved: true,
            },
            TruthAxis {
                label: "retention",
                terms: &["retention", "generations", "keep", "gzip"],
                resolved: true,
            },
            TruthAxis {
                label: "configuration",
                terms: &["config", "toml", "directories"],
                resolved: true,
            },
            TruthAxis {
                label: "runtime model",
                terms: &["daemon", "cron", "invocation", "runtime"],
                resolved: true,
            },
        ],
        scripted_reply: r#"{"axes":[
            {"axis":"rotation trigger","resolved":true,"evidence":"rotates any file over 100 MB","question":""},
            {"axis":"retention policy","resolved":true,"evidence":"keeps five gzip generations per file","question":""},
            {"axis":"configuration","resolved":true,"evidence":"directories listed in /etc/logrot.toml","question":""},
            {"axis":"runtime model","resolved":true,"evidence":"runs once per invocation — no daemon mode","question":""},
            {"axis":"file matching","resolved":false,"evidence":"not specified","question":"Are all files under the directories rotated, or only ones matching a pattern?"}
        ]}"#,
    },
    // --- under-specified: a brief from these encodes guesses ---
    Fixture {
        name: "snake-multiplayer",
        idea: "Take my terminal snake game and make it multiplayer.",
        well_specified: false,
        truth: &[
            TruthAxis {
                label: "base game carry-over",
                terms: &["existing", "carry", "terminal snake", "base game", "keep"],
                resolved: true,
            },
            TruthAxis {
                label: "player topology",
                terms: &[
                    "same screen",
                    "same-screen",
                    "remote",
                    "network",
                    "online",
                    "local",
                    "topology",
                    "players",
                ],
                resolved: false,
            },
            TruthAxis {
                label: "transport",
                terms: &["transport", "protocol", "tcp", "websocket", "connection"],
                resolved: false,
            },
            TruthAxis {
                label: "win condition",
                terms: &["win", "score", "scoring", "end", "round"],
                resolved: false,
            },
        ],
        scripted_reply: r#"{"axes":[
            {"axis":"base game carry-over","resolved":true,"evidence":"my terminal snake game","question":""},
            {"axis":"player topology","resolved":false,"evidence":"not specified","question":"Do players share one terminal, or connect remotely over a network?"},
            {"axis":"transport","resolved":false,"evidence":"not specified","question":"If remote, what connection/protocol should be used?"},
            {"axis":"win condition","resolved":false,"evidence":"not specified","question":"How does a multiplayer round end and who wins?"}
        ]}"#,
    },
    Fixture {
        name: "make-faster",
        idea: "The dashboard feels slow. Make the app faster.",
        well_specified: false,
        truth: &[
            TruthAxis {
                label: "target flows",
                terms: &["which", "flow", "page", "target", "where", "slow"],
                resolved: false,
            },
            TruthAxis {
                label: "success metric",
                terms: &["metric", "measure", "how fast", "budget", "goal", "latency"],
                resolved: false,
            },
            TruthAxis {
                label: "acceptable trade-offs",
                terms: &["trade", "constraint", "cost", "memory", "cache"],
                resolved: false,
            },
        ],
        scripted_reply: r#"{"axes":[
            {"axis":"target flows","resolved":false,"evidence":"not specified","question":"Which screens or interactions must get faster?"},
            {"axis":"success metric","resolved":false,"evidence":"not specified","question":"What latency or load-time target counts as fast enough?"},
            {"axis":"acceptable trade-offs","resolved":false,"evidence":"not specified","question":"Are caching, precomputation, or memory increases acceptable?"}
        ]}"#,
    },
    Fixture {
        name: "add-search",
        idea: "Add search to the notes app.",
        well_specified: false,
        truth: &[
            TruthAxis {
                label: "search kind",
                terms: &[
                    "keyword",
                    "semantic",
                    "full-text",
                    "fuzzy",
                    "kind",
                    "search work",
                ],
                resolved: false,
            },
            TruthAxis {
                label: "search scope",
                terms: &[
                    "scope",
                    "what is searched",
                    "titles",
                    "content",
                    "tags",
                    "fields",
                ],
                resolved: false,
            },
            TruthAxis {
                label: "results surface",
                terms: &["ui", "surface", "results", "display", "shown"],
                resolved: false,
            },
        ],
        scripted_reply: r#"{"axes":[
            {"axis":"search kind","resolved":false,"evidence":"not specified","question":"Should search be keyword matching or semantic similarity?"},
            {"axis":"search scope","resolved":false,"evidence":"not specified","question":"Which fields are searched — titles, content, tags?"},
            {"axis":"results surface","resolved":false,"evidence":"not specified","question":"Where are results shown and how are they ranked?"}
        ]}"#,
    },
    Fixture {
        name: "parser-rust",
        idea: "Rewrite the config parser in Rust.",
        well_specified: false,
        truth: &[
            TruthAxis {
                label: "rewrite scope",
                terms: &["scope", "config parser", "component", "which part"],
                resolved: true,
            },
            TruthAxis {
                label: "compatibility bar",
                terms: &[
                    "compatib",
                    "behaviour",
                    "behavior",
                    "parity",
                    "existing configs",
                    "accept",
                ],
                resolved: false,
            },
            TruthAxis {
                label: "error reporting",
                terms: &["error", "diagnostic", "message", "report"],
                resolved: false,
            },
            TruthAxis {
                label: "integration boundary",
                terms: &["integrat", "boundary", "ffi", "callers", "api"],
                resolved: false,
            },
        ],
        scripted_reply: r#"{"axes":[
            {"axis":"rewrite scope","resolved":true,"evidence":"the config parser","question":""},
            {"axis":"compatibility bar","resolved":false,"evidence":"not specified","question":"Must every config accepted today parse identically after the rewrite?"},
            {"axis":"error reporting","resolved":false,"evidence":"not specified","question":"What should parse errors look like — same messages as today or a new format?"},
            {"axis":"integration boundary","resolved":false,"evidence":"not specified","question":"How do existing callers reach the Rust parser — linked library, FFI, or a rewrite of the callers too?"}
        ]}"#,
    },
];

/// Semantic match for **recall**: any ground-truth term appearing in the
/// model axis's name *or question* (case-insensitive term overlap — the
/// cheap offline stand-in the corpus is calibrated against; recorded, not
/// assumed). Generous on purpose: recall asks "did the model surface this
/// decision at all?".
fn axis_matches(model_axis: &DecisionAxis, truth: &TruthAxis) -> bool {
    let haystack = format!("{} {}", model_axis.axis, model_axis.question).to_lowercase();
    truth.terms.iter().any(|term| haystack.contains(term))
}

/// Semantic match for **precision**: name only. A question naturally restates
/// vocabulary from neighbouring decisions ("does the *limit* apply to every
/// route?"), so matching questions here would cross-pair unrelated axes and
/// report phantom label disagreements.
fn axis_name_matches(model_axis: &DecisionAxis, truth: &TruthAxis) -> bool {
    let name = model_axis.axis.to_lowercase();
    truth.terms.iter().any(|term| name.contains(term))
}

/// Ground-truth recall over one assessment: every truth axis must be matched
/// by some model axis. Returns the labels of missed axes.
fn recall_misses(assessment: &GuidanceAssessment, fixture: &Fixture) -> Vec<&'static str> {
    fixture
        .truth
        .iter()
        .filter(|truth| !assessment.axes.iter().any(|axis| axis_matches(axis, truth)))
        .map(|truth| truth.label)
        .collect()
}

/// Ground-truth precision over one assessment: a matched axis must carry the
/// truth's resolved/not-specified label. Returns `(truth label, model axis)`
/// pairs that disagree.
fn precision_errors(
    assessment: &GuidanceAssessment,
    fixture: &Fixture,
) -> Vec<(&'static str, String)> {
    let mut errors = Vec::new();
    for truth in fixture.truth {
        for axis in &assessment.axes {
            if axis_name_matches(axis, truth) && axis.resolved != truth.resolved {
                errors.push((truth.label, axis.axis.clone()));
            }
        }
    }
    errors
}

async fn scripted_assessment(fixture: &Fixture) -> GuidanceAssessment {
    let provider = FakeProvider::new().text(fixture.scripted_reply);
    assess_guidance(&provider, "fixture-model", fixture.idea)
        .await
        .unwrap()
}

/// The offline ordering self-test: from the scripted responses, every
/// under-specified fixture scores strictly below every well-specified one.
/// A violation names the pair — refuse-to-trust, not a softened threshold.
#[tokio::test]
async fn ordering_selftest_under_specified_scores_strictly_below_well_specified() {
    let mut scored: Vec<(&Fixture, f32)> = Vec::new();
    for fixture in CORPUS {
        let assessment = scripted_assessment(fixture).await;
        scored.push((fixture, assessment.score));
    }
    for (under, under_score) in scored.iter().filter(|(f, _)| !f.well_specified) {
        for (well, well_score) in scored.iter().filter(|(f, _)| f.well_specified) {
            assert!(
                under_score < well_score,
                "guidance instrument cannot rank the corpus: under-specified '{}' scored \
                 {under_score:.2}, not strictly below well-specified '{}' at {well_score:.2} — \
                 do not trust the score until this is fixed",
                under.name,
                well.name,
            );
        }
    }
}

/// The corpus itself must satisfy the recall/precision checks over its own
/// scripted replies — fixture drift is caught the same way model drift is.
#[tokio::test]
async fn corpus_scripted_replies_are_consistent_with_ground_truth() {
    for fixture in CORPUS {
        let assessment = scripted_assessment(fixture).await;
        let misses = recall_misses(&assessment, fixture);
        assert!(
            misses.is_empty(),
            "fixture '{}': scripted reply misses ground-truth axes {misses:?}",
            fixture.name,
        );
        let errors = precision_errors(&assessment, fixture);
        assert!(
            errors.is_empty(),
            "fixture '{}': scripted reply mislabels axes {errors:?}",
            fixture.name,
        );
    }
}

/// Corpus composition guard: both classes stay represented, so the ordering
/// self-test can never pass vacuously.
#[test]
fn corpus_keeps_both_classes_represented() {
    let well = CORPUS.iter().filter(|f| f.well_specified).count();
    let under = CORPUS.iter().filter(|f| !f.well_specified).count();
    assert!(
        well >= 3 && under >= 3,
        "corpus must keep at least 3 fixtures per class (well={well}, under={under})"
    );
}

/// Live recall/precision drift check against a real provider. Opt-in and
/// never default CI; a deferred live run is the accepted bar (offline legs
/// above gate the merge). Run with a configured local model:
///   `LOCALPILOT_LIVE_TESTS=1 [LOCALPILOT_LIVE_MODEL=<model>] cargo test -p localpilot-harness --test guidance_instrument -- --nocapture`
#[test]
fn live_axis_recall_and_precision_over_the_corpus() {
    if std::env::var("LOCALPILOT_LIVE_TESTS").is_err() {
        eprintln!("skipping live guidance corpus check: set LOCALPILOT_LIVE_TESTS to enable");
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    let config = match localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(&cwd),
        &localpilot_config::CliOverrides::default(),
    ) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("skipping live guidance corpus check: config load failed: {err}");
            return;
        }
    };
    let registry = match localpilot_llm::ProviderRegistry::from_config(&config) {
        Ok(registry) => registry,
        Err(err) => {
            eprintln!("skipping live guidance corpus check: provider config incomplete: {err}");
            return;
        }
    };
    let Some(provider) = registry.default_provider().cloned() else {
        eprintln!("skipping live guidance corpus check: no default provider configured");
        return;
    };
    let Some(model) = std::env::var("LOCALPILOT_LIVE_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.resolve_model(None))
    else {
        eprintln!(
            "skipping live guidance corpus check: set provider.model or LOCALPILOT_LIVE_MODEL"
        );
        return;
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut recall_failures = Vec::new();
    let mut precision_failures = Vec::new();
    let mut ordering: Vec<(&Fixture, f32)> = Vec::new();
    for fixture in CORPUS {
        let assessment = rt
            .block_on(assess_guidance(provider.as_ref(), &model, fixture.idea))
            .expect("live assessment should parse within the retry cap");
        eprintln!(
            "live guidance '{}': score {:.2}, {} axes",
            fixture.name,
            assessment.score,
            assessment.axes.len()
        );
        let misses = recall_misses(&assessment, fixture);
        if !misses.is_empty() {
            recall_failures.push((fixture.name, misses));
        }
        let errors = precision_errors(&assessment, fixture);
        if !errors.is_empty() {
            precision_failures.push((fixture.name, errors));
        }
        ordering.push((fixture, assessment.score));
    }
    for (under, under_score) in ordering.iter().filter(|(f, _)| !f.well_specified) {
        for (well, well_score) in ordering.iter().filter(|(f, _)| f.well_specified) {
            if under_score >= well_score {
                eprintln!(
                    "live ordering violation: '{}' ({under_score:.2}) >= '{}' ({well_score:.2})",
                    under.name, well.name
                );
            }
        }
    }
    // A recall miss is the dangerous failure: an unlisted axis silently
    // inflates the score. It fails the fixture regardless of the score.
    assert!(
        recall_failures.is_empty(),
        "live axis recall failed (model={model}): {recall_failures:?}"
    );
    assert!(
        precision_failures.is_empty(),
        "live axis precision failed (model={model}): {precision_failures:?}"
    );
}
