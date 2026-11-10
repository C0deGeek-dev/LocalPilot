//! Choosing task guidance from what the user asked and what the project is.
//!
//! The selector is deterministic and makes no model call. It reads typed
//! inputs only: the user's own instruction (never a tool result or a quoted
//! log), an explicit skill the user named, the facts of the component being
//! worked on, the eligible skill catalog and the capabilities available. It
//! returns one primary procedure, at most two supplements, metadata-only
//! suggestions, and every exclusion with its reason.
//!
//! The rules, in order:
//!
//! 1. **Eligibility.** A user-only package is reachable only by the user naming
//!    it. An untrusted project overlay is already absent from the catalog.
//! 2. **Intent** comes from the user's words, English or Dutch, with quoted
//!    material removed and negation honoured. Several intents become ordered
//!    phases; a defect fix is investigation, then implementation.
//! 3. **Component.** A path the user named wins, then the paths the work is
//!    touching; conflicting components leave the choice tentative. The root is
//!    a fallback only.
//! 4. **Stack and product** come from the component's facts. Engine evidence
//!    outranks a generated language project file.
//! 5. **Match.** A procedure matches the primary intent; a supplement matches
//!    the component's stacks or products. Explanation and review leave out
//!    product supplements, which are about running things.
//! 6. **Missing capabilities** narrow the claims a supplement can support; they
//!    never remove it or block the task.
//! 7. **Loading.** A body may be loaded only when the intent and the component
//!    are clear and the primary procedure is unambiguous; otherwise selection
//!    falls back to metadata.
//!
//! Confidence is categorical, never a probability: confirmed, declared or
//! tentative, each with the reason shown.

use std::collections::BTreeSet;
use std::path::Path;

use localpilot_skills::{GuidanceRole, Skill, SkillSet};

use crate::{ComponentFacts, FactConfidence, ProjectFacts};

/// Supplements loaded beside the primary procedure, at most.
pub const MAX_SUPPLEMENTS: usize = 2;
/// Metadata-only suggestions offered, at most.
pub const MAX_SUGGESTIONS: usize = 3;

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaskIntent {
    Explain,
    Investigate,
    Implement,
    Refactor,
    Review,
    Measure,
    Package,
}

impl TaskIntent {
    /// The name used in skill metadata.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explain => "explain",
            Self::Investigate => "investigate",
            Self::Implement => "implement",
            Self::Refactor => "refactor",
            Self::Review => "review",
            Self::Measure => "measure",
            Self::Package => "package",
        }
    }

    /// Whether the intent changes code or runs the application.
    #[must_use]
    pub fn acts(self) -> bool {
        !matches!(self, Self::Explain | Self::Review)
    }
}

/// How sure a selection is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SelectionConfidence {
    /// Established by the user's words or by project evidence.
    Confirmed,
    /// Said by the user about something that does not exist yet.
    Declared,
    /// Inferred, ambiguous or a fallback.
    Tentative,
}

/// The intent phases of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentReading {
    /// In order; the first is the one guidance starts with.
    pub phases: Vec<TaskIntent>,
    pub confidence: SelectionConfidence,
    pub reason: String,
}

impl IntentReading {
    /// The intent guidance starts with.
    #[must_use]
    pub fn primary(&self) -> Option<TaskIntent> {
        self.phases.first().copied()
    }
}

/// The component a task is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentReading {
    pub component: String,
    /// Other components the evidence also pointed at.
    pub alternatives: Vec<String>,
    pub confidence: SelectionConfidence,
    pub reason: String,
}

/// A chosen package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedGuidance {
    pub name: String,
    pub reason: String,
    /// Capabilities the package needs for a claim that are not available.
    /// Claims depending on them are unsupported; the guidance still applies.
    pub unsupported_claims: Vec<String>,
    /// Every capability a claim made under this guidance needs, available or
    /// not: the evidence it asks for.
    pub required_for_claim: Vec<String>,
}

/// A package that was considered and left out, with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedGuidance {
    pub name: String,
    pub reason: String,
}

/// A reviewed project run recipe chosen for the component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedRecipe {
    /// The package that declares it, and the recipe's name in it.
    pub package: String,
    pub name: String,
    pub summary: String,
    pub validation: localpilot_skills::RecipeValidation,
    pub reason: String,
}

impl SelectedRecipe {
    /// `package/name`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}/{}", self.package, self.name)
    }
}

/// Whether selection may load instruction bodies or only offer metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadDecision {
    Body,
    Metadata,
}

/// The result of one selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceSelection {
    pub intent: IntentReading,
    pub component: ComponentReading,
    /// The stacks and products the component's facts established, with the
    /// confidence of the weakest fact behind each.
    pub stacks: Vec<(String, SelectionConfidence)>,
    pub products: Vec<(String, SelectionConfidence)>,
    pub primary: Option<SelectedGuidance>,
    pub supplements: Vec<SelectedGuidance>,
    /// Relevant packages offered by name and summary only.
    pub suggestions: Vec<SelectedGuidance>,
    pub excluded: Vec<ExcludedGuidance>,
    /// Reviewed run recipes from project packages for this component.
    pub recipes: Vec<SelectedRecipe>,
    pub loading: LoadDecision,
    pub loading_reason: String,
}

impl GuidanceSelection {
    /// The names of the primary procedure and the supplements, in order.
    #[must_use]
    pub fn selected_names(&self) -> Vec<&str> {
        self.primary
            .iter()
            .chain(&self.supplements)
            .map(|selected| selected.name.as_str())
            .collect()
    }
}

/// The inputs of one selection.
pub struct SelectionInput<'a> {
    /// The user's own instruction. Never tool output or quoted material.
    pub request: &'a str,
    /// A package the user named explicitly, if any.
    pub explicit: Option<&'a str>,
    /// Workspace-relative paths the work is touching, most recent last.
    pub paths: &'a [String],
    /// Observation capabilities available in this session.
    pub capabilities: &'a BTreeSet<String>,
    /// The user's earlier instructions in this session, oldest first. When the
    /// request itself names no intent ("go on", "ga door"), the most recent of
    /// these that does supplies it: the task in progress continues.
    pub earlier: &'a [String],
}

/// Select guidance for `input` from `catalog`, reading component facts from
/// `facts`.
pub fn select_guidance(
    input: &SelectionInput<'_>,
    facts: &mut ProjectFacts,
    catalog: &SkillSet,
) -> GuidanceSelection {
    let mut intent = read_intent(input.request);
    if intent.confidence == SelectionConfidence::Tentative {
        if let Some((earlier, reading)) = input
            .earlier
            .iter()
            .rev()
            .map(|earlier| (earlier, read_intent(earlier)))
            .find(|(_, reading)| reading.confidence != SelectionConfidence::Tentative)
        {
            intent = IntentReading {
                reason: format!(
                    "this request names no intent; it continues the earlier request \"{}\"",
                    localpilot_core::one_line(earlier, 80)
                ),
                ..reading
            };
        }
    }
    let component = read_component(input, facts);
    let component_facts = facts.facts_for(&component.component);
    let stacks = stacks_of(&component_facts);
    let products = products_of(&component_facts);
    let mut excluded = Vec::new();

    // Explicit selection wins, and is the only way to reach a user-only package.
    let explicit = input.explicit.and_then(|name| {
        let found = catalog.by_name(name.trim());
        if found.is_none() {
            excluded.push(ExcludedGuidance {
                name: name.trim().to_string(),
                reason: "named by the user, but no installed package has that name".to_string(),
            });
        }
        found
    });

    let mut procedures: Vec<(u32, &Skill, String)> = Vec::new();
    let mut supplements: Vec<(u32, &Skill, SelectedGuidance)> = Vec::new();
    let text_scores: Vec<(&Skill, u32)> = catalog.ranked(input.request);
    let text_score = |skill: &Skill| {
        text_scores
            .iter()
            .find(|(candidate, _)| candidate.manifest.name == skill.manifest.name)
            .map_or(0, |(_, score)| *score)
    };

    for skill in catalog.discoverable() {
        if explicit.is_some_and(|chosen| chosen.manifest.name == skill.manifest.name) {
            continue;
        }
        let name = skill.manifest.name.clone();
        let metadata = match &skill.guidance {
            localpilot_skills::Applicability::Valid(metadata) => metadata,
            localpilot_skills::Applicability::Invalid(reason) => {
                excluded.push(ExcludedGuidance {
                    name,
                    reason: format!("its [guidance] section cannot be used: {reason}"),
                });
                continue;
            }
            // Packages without metadata take part only as text suggestions.
            localpilot_skills::Applicability::Absent => continue,
        };
        let Some(primary) = intent.primary() else {
            continue;
        };
        if metadata
            .exclude_intents
            .iter()
            .any(|i| i == primary.as_str())
        {
            excluded.push(ExcludedGuidance {
                name,
                reason: format!("it excludes the intent {}", primary.as_str()),
            });
            continue;
        }
        if !metadata.component_globs.is_empty()
            && !glob_matches(&metadata.component_globs, &component.component)
        {
            excluded.push(ExcludedGuidance {
                name,
                reason: format!(
                    "its component globs do not match the component {}",
                    component.component
                ),
            });
            continue;
        }
        match metadata.role {
            GuidanceRole::Procedure => {
                if metadata.intents.iter().any(|i| i == primary.as_str()) {
                    procedures.push((
                        1000 + text_score(skill) * 2 + own(skill),
                        skill,
                        format!("procedure for the intent {}", primary.as_str()),
                    ));
                } else {
                    excluded.push(ExcludedGuidance {
                        name,
                        reason: format!("a procedure for other intents than {}", primary.as_str()),
                    });
                }
            }
            GuidanceRole::Supplement => {
                if !metadata.intents.is_empty()
                    && !metadata.intents.iter().any(|i| i == primary.as_str())
                {
                    excluded.push(ExcludedGuidance {
                        name,
                        reason: format!("a supplement for other intents than {}", primary.as_str()),
                    });
                    continue;
                }
                let stack_hits: Vec<&str> = metadata
                    .stacks
                    .iter()
                    .filter(|s| stacks.iter().any(|(have, _)| have == *s))
                    .map(String::as_str)
                    .collect();
                let product_hits: Vec<&str> = metadata
                    .products
                    .iter()
                    .filter(|p| products.iter().any(|(have, _)| have == *p))
                    .map(String::as_str)
                    .collect();
                let product_only = metadata.stacks.is_empty() && !metadata.products.is_empty();
                if product_only && !primary.acts() {
                    excluded.push(ExcludedGuidance {
                        name,
                        reason: format!(
                            "{} does not run or change the {} product, so its runtime guidance is left out",
                            primary.as_str(),
                            metadata.products.join("/")
                        ),
                    });
                    continue;
                }
                // A supplement that names both stacks and products needs a
                // stack match; product alone would pull in another stack.
                let matches = if metadata.stacks.is_empty() {
                    !product_hits.is_empty()
                } else {
                    !stack_hits.is_empty()
                        && (metadata.products.is_empty()
                            || !product_hits.is_empty()
                            || !primary.acts())
                };
                if !matches {
                    excluded.push(ExcludedGuidance {
                        name,
                        reason: format!(
                            "the component {} shows no {}",
                            component.component,
                            describe_needs(&metadata.stacks, &metadata.products)
                        ),
                    });
                    continue;
                }
                let unsupported_claims: Vec<String> = metadata
                    .capabilities_required_for_claim
                    .iter()
                    .filter(|cap| !input.capabilities.contains(*cap))
                    .cloned()
                    .collect();
                let hits: Vec<&str> = stack_hits.iter().chain(&product_hits).copied().collect();
                let score = u32::try_from(hits.len()).unwrap_or(u32::MAX) * 1000
                    + text_score(skill) * 2
                    + own(skill);
                supplements.push((
                    score,
                    skill,
                    SelectedGuidance {
                        name,
                        reason: format!("the component shows {}", hits.join(", ")),
                        unsupported_claims,
                        required_for_claim: metadata.capabilities_required_for_claim.clone(),
                    },
                ));
            }
        }
    }

    procedures.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.manifest.name.cmp(&b.1.manifest.name))
    });
    supplements.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.manifest.name.cmp(&b.1.manifest.name))
    });

    let tied = procedures.len() > 1 && procedures[0].0 == procedures[1].0;
    let primary = match explicit {
        Some(skill) => Some(SelectedGuidance {
            name: skill.manifest.name.clone(),
            reason: "named explicitly by the user".to_string(),
            unsupported_claims: Vec::new(),
            required_for_claim: Vec::new(),
        }),
        None => procedures
            .first()
            .map(|(_, skill, reason)| SelectedGuidance {
                name: skill.manifest.name.clone(),
                reason: reason.clone(),
                unsupported_claims: Vec::new(),
                required_for_claim: Vec::new(),
            }),
    };
    for (_, skill, _) in procedures.iter().skip(1) {
        excluded.push(ExcludedGuidance {
            name: skill.manifest.name.clone(),
            reason: "another procedure for the same intent ranked higher".to_string(),
        });
    }
    let mut chosen_supplements = Vec::new();
    for (index, (_, _, selected)) in supplements.into_iter().enumerate() {
        if index < MAX_SUPPLEMENTS {
            chosen_supplements.push(selected);
        } else {
            excluded.push(ExcludedGuidance {
                name: selected.name,
                reason: format!("more than {MAX_SUPPLEMENTS} supplements matched; it ranked lower"),
            });
        }
    }

    // Packages without metadata, and anything when the intent is unclear, are
    // offered as suggestions by text relevance.
    let selected: BTreeSet<String> = primary
        .iter()
        .chain(&chosen_supplements)
        .map(|s| s.name.clone())
        .collect();
    let suggestions = text_scores
        .iter()
        .filter(|(skill, _)| !selected.contains(&skill.manifest.name))
        .filter(|(skill, _)| {
            matches!(skill.guidance, localpilot_skills::Applicability::Absent)
                || intent.primary().is_none()
        })
        .take(MAX_SUGGESTIONS)
        .map(|(skill, score)| SelectedGuidance {
            name: skill.manifest.name.clone(),
            reason: format!("its description matches the request (score {score})"),
            unsupported_claims: Vec::new(),
            required_for_claim: Vec::new(),
        })
        .collect();

    let (loading, loading_reason) = if explicit.is_some() {
        (LoadDecision::Body, "the user named the package".to_string())
    } else if intent.confidence == SelectionConfidence::Tentative {
        (
            LoadDecision::Metadata,
            format!("the intent is unclear: {}", intent.reason),
        )
    } else if component.confidence == SelectionConfidence::Tentative {
        (
            LoadDecision::Metadata,
            format!("the component is unclear: {}", component.reason),
        )
    } else if primary.is_none() {
        (
            LoadDecision::Metadata,
            "no procedure matches the intent".to_string(),
        )
    } else if tied {
        (
            LoadDecision::Metadata,
            "two procedures match equally; an arbitrary tie does not justify loading one"
                .to_string(),
        )
    } else {
        (
            LoadDecision::Body,
            "intent and component are clear and one procedure matches".to_string(),
        )
    };

    let recipes = select_recipes(
        catalog,
        &component.component,
        intent.primary(),
        facts.root(),
        &mut excluded,
    );

    excluded.sort_by(|a, b| a.name.cmp(&b.name));
    excluded.dedup_by(|a, b| a.name == b.name);
    GuidanceSelection {
        intent,
        component,
        stacks,
        products,
        primary,
        supplements: chosen_supplements,
        suggestions,
        excluded,
        recipes,
        loading,
        loading_reason,
    }
}

/// Recipes from trusted project packages whose component is `component`. A
/// global or built-in package names no project, so its recipes never apply.
/// Explanation and review do not run anything, so they get none.
fn select_recipes(
    catalog: &SkillSet,
    component: &str,
    intent: Option<TaskIntent>,
    root: &Path,
    excluded: &mut Vec<ExcludedGuidance>,
) -> Vec<SelectedRecipe> {
    let mut out = Vec::new();
    for skill in catalog.discoverable() {
        let project = matches!(
            skill.scope,
            localpilot_skills::SkillScope::ProjectLocalPilot
                | localpilot_skills::SkillScope::ProjectAgents
        );
        for invalid in &skill.recipes.invalid {
            excluded.push(ExcludedGuidance {
                name: format!(
                    "{}/{}",
                    skill.manifest.name,
                    invalid.split(':').next().unwrap_or_default()
                ),
                reason: format!("its recipe cannot be used: {invalid}"),
            });
        }
        for recipe in &skill.recipes.recipes {
            let id = format!("{}/{}", skill.manifest.name, recipe.name);
            if !project {
                excluded.push(ExcludedGuidance {
                    name: id,
                    reason: "a recipe outside a project package names no project".to_string(),
                });
                continue;
            }
            if recipe.component != component {
                continue;
            }
            if !intent.is_some_and(TaskIntent::acts) {
                excluded.push(ExcludedGuidance {
                    name: id,
                    reason: "this request does not run anything, so run recipes are left out"
                        .to_string(),
                });
                continue;
            }
            out.push(SelectedRecipe {
                package: skill.manifest.name.clone(),
                name: recipe.name.clone(),
                summary: recipe.summary(),
                validation: recipe.validation(root),
                reason: format!(
                    "reviewed recipe for component {component} from {} ({})",
                    skill.manifest.name,
                    skill.scope.label()
                ),
            });
        }
    }
    out
}

/// One point for a package the user or the project installed, so on an
/// otherwise equal match it outranks a built-in one.
fn own(skill: &Skill) -> u32 {
    u32::from(skill.scope != localpilot_skills::SkillScope::Builtin)
}

fn describe_needs(stacks: &[String], products: &[String]) -> String {
    match (stacks.is_empty(), products.is_empty()) {
        (false, true) => format!("stack {}", stacks.join("/")),
        (true, false) => format!("product {}", products.join("/")),
        _ => format!(
            "stack {} with product {}",
            stacks.join("/"),
            products.join("/")
        ),
    }
}

fn glob_matches(globs: &[String], component: &str) -> bool {
    let mut builder = globset::GlobSetBuilder::new();
    for glob in globs {
        if let Ok(glob) = globset::Glob::new(glob) {
            builder.add(glob);
        }
    }
    builder
        .build()
        .is_ok_and(|set| set.is_match(component) || set.is_match(format!("{component}/x")))
}

/// Words that mark each intent, English and Dutch, matched as whole words or
/// phrases in the lowercased request.
const INTENT_WORDS: &[(TaskIntent, &[&str])] = &[
    (
        TaskIntent::Explain,
        &[
            "explain",
            "how does",
            "how do",
            "what does",
            "walk me through",
            "describe",
            "leg uit",
            "leg eens uit",
            "uitleg",
            "hoe werkt",
            "wat doet",
            "beschrijf",
        ],
    ),
    (
        TaskIntent::Investigate,
        &[
            "investigate",
            "find out why",
            "figure out why",
            "why does",
            "why is",
            "debug",
            "diagnose",
            "root cause",
            "onderzoek",
            "zoek uit",
            "waarom",
            "achterhaal",
        ],
    ),
    (
        TaskIntent::Implement,
        &[
            "implement",
            "add",
            "build a",
            "build an",
            "create",
            "make a",
            "make an",
            "support",
            "voeg toe",
            "toevoegen",
            "bouw",
            "maak",
            "implementeer",
            "ondersteun",
        ],
    ),
    (
        TaskIntent::Refactor,
        &[
            "refactor",
            "restructure",
            "clean up",
            "split up",
            "extract",
            "rename",
            "herstructureer",
            "ruim op",
            "opschonen",
            "splits",
            "hernoem",
        ],
    ),
    (
        TaskIntent::Review,
        &[
            "review",
            "check this change",
            "check the change",
            "look over",
            "audit",
            "controleer deze wijziging",
            "controleer de wijziging",
            "beoordeel",
            "nakijken",
            "kijk na",
        ],
    ),
    (
        TaskIntent::Measure,
        &[
            "measure",
            "benchmark",
            "profile",
            "how fast",
            "performance",
            "latency",
            "meet",
            "prestaties",
            "hoe snel",
            "snelheid",
        ],
    ),
    (
        TaskIntent::Package,
        &[
            "publish",
            "package it",
            "package the app",
            "cut a release",
            "publiceer",
            "verpak",
        ],
    ),
];

/// Words for a defect fix: investigation first, then implementation.
const FIX_WORDS: &[&str] = &[
    "fix", "repair", "resolve", "solve", "los op", "los de", "los het", "repareer", "herstel",
    "oplossen",
];

/// Dutch separable verbs: the verb, its particle later in the same sentence,
/// and the intent (`None` for a defect fix).
const SEPARABLE: &[(&str, &str, Option<TaskIntent>)] = &[
    ("voeg", "toe", Some(TaskIntent::Implement)),
    ("pas", "aan", Some(TaskIntent::Implement)),
    ("leg", "uit", Some(TaskIntent::Explain)),
    ("zoek", "uit", Some(TaskIntent::Investigate)),
    ("ruim", "op", Some(TaskIntent::Refactor)),
    ("kijk", "na", Some(TaskIntent::Review)),
    ("los", "op", None),
];

/// Where `verb` starts when `particle` follows it in the same sentence.
fn separable(text: &str, verb: &str, particle: &str) -> Option<usize> {
    let at = find_phrase(text, verb)?;
    let rest = &text[at + verb.len()..];
    let sentence = rest
        .split(['.', '!', '?', ';', '\n'])
        .next()
        .unwrap_or_default();
    find_phrase(sentence, particle).map(|_| at)
}

/// Phrases that forbid changing anything.
const NO_CHANGE_WORDS: &[&str] = &[
    "don't change",
    "do not change",
    "without changing",
    "no changes",
    "don't edit",
    "do not edit",
    "read-only",
    "read only",
    "verander niets",
    "niets aanpassen",
    "niets wijzigen",
    "zonder te wijzigen",
    "zonder aan te passen",
    "pas niets aan",
];

/// Read the intent phases of `request`.
#[must_use]
pub fn read_intent(request: &str) -> IntentReading {
    let text = format!(" {} ", strip_quoted(request).to_lowercase());
    let no_change = NO_CHANGE_WORDS
        .iter()
        .any(|phrase| contains_phrase(&text, phrase));
    let mut found: Vec<(usize, TaskIntent)> = Vec::new();
    for (intent, words) in INTENT_WORDS {
        if let Some(at) = words
            .iter()
            .filter_map(|word| find_phrase(&text, word))
            .min()
        {
            found.push((at, *intent));
        }
    }
    // Dutch separable verbs: "voeg een vlag toe", "los de fout op".
    for (verb, particle, intent) in SEPARABLE {
        if let Some(at) = separable(&text, verb, particle) {
            match intent {
                Some(intent) => found.push((at, *intent)),
                None => {
                    found.push((at, TaskIntent::Investigate));
                    found.push((at + 1, TaskIntent::Implement));
                }
            }
        }
    }
    if let Some(at) = FIX_WORDS
        .iter()
        .filter_map(|word| find_phrase(&text, word))
        .min()
    {
        found.push((at, TaskIntent::Investigate));
        found.push((at + 1, TaskIntent::Implement));
    }
    found.sort();
    let mut phases: Vec<TaskIntent> = Vec::new();
    for (_, intent) in found {
        if no_change && matches!(intent, TaskIntent::Implement | TaskIntent::Refactor) {
            continue;
        }
        if !phases.contains(&intent) {
            phases.push(intent);
        }
    }
    // An explanation or review asked for on its own does not imply the
    // "make"/"add" verbs it may mention as its subject.
    if matches!(
        phases.first(),
        Some(TaskIntent::Explain | TaskIntent::Review)
    ) {
        phases.retain(|intent| !intent.acts() || *intent == TaskIntent::Investigate);
    }
    match phases.len() {
        0 => IntentReading {
            phases,
            confidence: SelectionConfidence::Tentative,
            reason: "no intent word in the request".to_string(),
        },
        _ => IntentReading {
            reason: format!(
                "the request asks to {}{}",
                phases
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", then "),
                if no_change {
                    " without changing anything"
                } else {
                    ""
                }
            ),
            phases,
            confidence: SelectionConfidence::Confirmed,
        },
    }
}

/// Read the component from a path the user named, then the paths in play.
fn read_component(input: &SelectionInput<'_>, facts: &ProjectFacts) -> ComponentReading {
    let named: Vec<String> = path_tokens(&strip_quoted(input.request))
        .into_iter()
        .filter_map(|token| {
            let path = Path::new(&token);
            let exists = facts.root().join(path).exists();
            exists.then(|| facts.component_of(path)).flatten()
        })
        .collect();
    if let Some(reading) = choose(&named, "the request names") {
        return reading;
    }
    let touched: Vec<String> = input
        .paths
        .iter()
        .filter_map(|path| facts.component_of(Path::new(path)))
        .collect();
    if let Some(reading) = choose(&touched, "the work touches") {
        return reading;
    }
    ComponentReading {
        component: ".".to_string(),
        alternatives: Vec::new(),
        confidence: if facts.root().join(".").exists() && is_single_component(facts) {
            SelectionConfidence::Confirmed
        } else {
            SelectionConfidence::Tentative
        },
        reason: "no path names a component; the workspace root is used".to_string(),
    }
}

/// A workspace with one component at its root is unambiguous.
fn is_single_component(facts: &ProjectFacts) -> bool {
    facts.components().len() == 1
}

fn choose(components: &[String], how: &str) -> Option<ComponentReading> {
    let mut distinct: Vec<String> = Vec::new();
    for component in components {
        if !distinct.contains(component) {
            distinct.push(component.clone());
        }
    }
    let first = distinct.first()?.clone();
    if distinct.len() == 1 {
        return Some(ComponentReading {
            reason: format!("{how} a path in {first}"),
            component: first,
            alternatives: Vec::new(),
            confidence: SelectionConfidence::Confirmed,
        });
    }
    // The most recent one leads; the others stay visible.
    let last = distinct.last()?.clone();
    Some(ComponentReading {
        reason: format!("{how} paths in {} components", distinct.len()),
        alternatives: distinct.into_iter().filter(|c| *c != last).collect(),
        component: last,
        confidence: SelectionConfidence::Tentative,
    })
}

/// Stacks the facts establish. A generated project file beside an engine
/// project adds no .NET stack.
#[must_use]
pub fn stacks_of(facts: &ComponentFacts) -> Vec<(String, SelectionConfidence)> {
    let engine =
        facts.has("godot.project") || facts.has("unity.project") || facts.has("unreal.project");
    let mut out: Vec<(String, SelectionConfidence)> = Vec::new();
    let mut add = |stack: &str, confidence: FactConfidence| {
        let confidence = to_selection(confidence);
        match out.iter_mut().find(|(have, _)| have == stack) {
            Some(entry) => entry.1 = entry.1.min(confidence),
            None => out.push((stack.to_string(), confidence)),
        }
    };
    for fact in &facts.facts {
        let id = fact.id.as_str();
        let declared = id.strip_prefix("declared.");
        let stacks: &[&str] = match declared.unwrap_or(id) {
            "cargo.package" | "cargo.workspace" | "cargo.manifest" | "rust" => &["rust"],
            "dotnet.sdk_project" if !engine => &["dotnet"],
            "dotnet" | "csharp" => &["dotnet"],
            "aspnet.project" | "aspnet" => &["dotnet", "aspnet"],
            "wpf.project" | "wpf" => &["dotnet", "wpf"],
            "avalonia.project" | "avalonia" => &["dotnet", "avalonia"],
            "godot.csharp" => &["csharp"],
            "node.package" | "node" => &["node"],
            "dependency.react" | "react" => &["web", "react"],
            "dependency.vue" | "dependency.svelte" | "dependency.next" | "dependency.vite"
            | "web" => &["web"],
            "dependency.three" | "dependency.@react-three/fiber" | "threejs" => &["web", "threejs"],
            "dependency.tailwindcss" | "tailwind" => &["tailwind"],
            "godot.project" | "godot" => &["godot"],
            "unity.project" | "unity" => &["unity"],
            "unreal.project" | "unreal" => &["unreal"],
            "python.project" | "python" => &["python"],
            "go.module" | "go" => &["go"],
            _ => &[],
        };
        for stack in stacks {
            add(stack, fact.confidence);
        }
        if id == "godot.version" && fact.value.as_deref().is_some_and(|v| v.starts_with('4')) {
            add("godot4", fact.confidence);
        }
    }
    out.sort();
    out
}

/// Products the facts establish.
#[must_use]
pub fn products_of(facts: &ComponentFacts) -> Vec<(String, SelectionConfidence)> {
    let mut out: Vec<(String, SelectionConfidence)> = Vec::new();
    for fact in &facts.facts {
        let id = fact.id.strip_prefix("declared.").unwrap_or(&fact.id);
        let product = id.strip_prefix("product.").unwrap_or(id);
        if !localpilot_skills::PRODUCTS.contains(&product) {
            continue;
        }
        let confidence = to_selection(fact.confidence);
        match out.iter_mut().find(|(have, _)| have == product) {
            Some(entry) => entry.1 = entry.1.min(confidence),
            None => out.push((product.to_string(), confidence)),
        }
    }
    out.sort();
    out
}

fn to_selection(confidence: FactConfidence) -> SelectionConfidence {
    match confidence {
        FactConfidence::Confirmed => SelectionConfidence::Confirmed,
        FactConfidence::Declared => SelectionConfidence::Declared,
        FactConfidence::Tentative => SelectionConfidence::Tentative,
    }
}

/// `request` with quoted material blanked: code fences, inline code, and text
/// in double quotes. Quoted text is data, not an instruction.
fn strip_quoted(request: &str) -> String {
    let mut out = String::with_capacity(request.len());
    let mut in_fence = false;
    for line in request.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if in_fence || line.trim_start().starts_with('>') {
            out.push('\n');
            continue;
        }
        let mut quoted: Option<char> = None;
        for c in line.chars() {
            match quoted {
                Some(close) if c == close => quoted = None,
                Some(_) => {}
                None if matches!(c, '`' | '"' | '“') => {
                    quoted = Some(if c == '“' { '”' } else { c });
                }
                None => out.push(c),
            }
        }
        out.push('\n');
    }
    out
}

fn find_phrase(text: &str, phrase: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(found) = text[start..].find(phrase) {
        let at = start + found;
        let before = text[..at].chars().next_back();
        let after = text[at + phrase.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
        if boundary(before) && boundary(after) {
            return Some(at);
        }
        start = at + phrase.len().max(1);
    }
    None
}

fn contains_phrase(text: &str, phrase: &str) -> bool {
    find_phrase(text, phrase).is_some()
}

/// Path-shaped words: containing `/` or a `.` with an extension.
fn path_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| {
                !c.is_alphanumeric() && !matches!(c, '/' | '\\' | '.' | '_' | '-')
            })
            .trim_end_matches('.')
            .replace('\\', "/")
        })
        .filter(|word| {
            !word.is_empty() && (word.contains('/') || word.contains('.')) && !word.starts_with('/')
        })
        .take(32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phases(request: &str) -> Vec<&'static str> {
        read_intent(request)
            .phases
            .iter()
            .map(|p| p.as_str())
            .collect()
    }

    #[test]
    fn english_and_dutch_requests_read_the_same_intents() {
        let pairs = [
            (
                "Explain how this parser works.",
                "Leg uit hoe deze parser werkt.",
                vec!["explain"],
            ),
            (
                "Investigate why the build is slow.",
                "Zoek uit waarom de build traag is.",
                vec!["investigate"],
            ),
            (
                "Add a --json flag.",
                "Voeg een --json vlag toe.",
                vec!["implement"],
            ),
            (
                "Refactor the config loader.",
                "Herstructureer de config loader.",
                vec!["refactor"],
            ),
            (
                "Review this change.",
                "Controleer deze wijziging.",
                vec!["review"],
            ),
            (
                "Measure the startup time.",
                "Meet de opstarttijd.",
                vec!["measure"],
            ),
            (
                "Fix the wrong exit code.",
                "Los de verkeerde exitcode op.",
                vec!["investigate", "implement"],
            ),
        ];
        for (english, dutch, expected) in pairs {
            assert_eq!(phases(english), expected, "{english}");
            assert_eq!(phases(dutch), expected, "{dutch}");
        }
    }

    #[test]
    fn quoted_material_and_negation_do_not_add_intents() {
        assert_eq!(phases("Explain what `fix_all()` does."), ["explain"]);
        assert_eq!(
            phases("Explain the error \"failed to build\"."),
            ["explain"]
        );
        assert_eq!(
            phases("Investigate the crash but don't change anything."),
            ["investigate"]
        );
        assert_eq!(
            phases("Zoek uit waarom het crasht, verander niets."),
            ["investigate"]
        );
        assert_eq!(
            phases("Explain how we make the release build."),
            ["explain"]
        );
    }

    #[test]
    fn several_intents_become_ordered_phases() {
        assert_eq!(
            phases("Refactor the loader and then measure it."),
            ["refactor", "measure"]
        );
    }

    #[test]
    fn a_request_without_an_intent_word_is_tentative() {
        let reading = read_intent("the settings page");
        assert!(reading.phases.is_empty());
        assert_eq!(reading.confidence, SelectionConfidence::Tentative);
    }

    #[test]
    fn a_word_inside_another_word_is_not_a_match() {
        // "address" contains "add"; "prefix" contains "fix".
        assert!(phases("the address prefix").is_empty());
    }
}
