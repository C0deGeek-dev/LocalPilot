//! Pre-brief guidance assessment: how much load-bearing product guidance does
//! an idea actually contain?
//!
//! The model enumerates the idea's *decision axes* — the small set of product
//! decisions that would change what gets built — and marks each one resolved
//! (quoting the idea's own words that settle it) or not specified. The axes
//! are the model's, invented per idea; nothing is predefined per domain. The
//! score is deterministic: resolved axes over total axes, `1.0` when no axes
//! are found (a trivial idea is not penalized).
//!
//! The score is a *signal with a known failure mode*, never a safety claim:
//! an axis the model fails to list at all cannot count against the score, so
//! a confidently wrong high score is possible. Callers must expose the full
//! axis list for human inspection alongside the number. Gating policy — what
//! to do below a threshold — belongs to the caller; this module only
//! assesses.

use serde::{Deserialize, Serialize};

use localpilot_core::{Message, Role};
use localpilot_llm::ModelProvider;

use crate::error::HarnessError;
use crate::planning::generate;

/// The original LocalPilot guidance-assessment prompt.
pub const GUIDANCE_PROMPT: &str = "\
You are the guidance auditor for a software project intake. Before a brief is \
written from the user's idea, identify the idea's decision axes: the small set \
of product decisions that would materially change what gets built. Find the \
axes that matter for THIS idea — scope boundaries, platform or runtime, \
interaction model, data handling, and the like are only examples of where to \
look, not a list to copy.\n\
\n\
Respond with ONLY a JSON object in exactly this shape, and nothing else:\n\
\n\
{\n\
  \"axes\": [\n\
    {\n\
      \"axis\": \"<short name of the decision>\",\n\
      \"resolved\": true,\n\
      \"evidence\": \"<the idea's own words that settle it, quoted verbatim>\",\n\
      \"question\": \"\"\n\
    },\n\
    {\n\
      \"axis\": \"<short name of the decision>\",\n\
      \"resolved\": false,\n\
      \"evidence\": \"not specified\",\n\
      \"question\": \"<one concrete question whose answer settles this axis>\"\n\
    }\n\
  ]\n\
}\n\
\n\
Rules: an axis counts as resolved only when the idea's own words settle it — \
quote those words verbatim in evidence. When the idea does not settle an axis, \
resolved is false, evidence is \"not specified\", and question asks for exactly \
the missing decision. List the most consequential axes first. A trivial or \
fully specified idea may have few axes or none; never invent filler axes.";

/// One product decision the idea must settle for a brief to be unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionAxis {
    /// Short name of the decision (e.g. which rendering paradigm to keep).
    pub axis: String,
    /// Whether the idea's own words settle this decision.
    pub resolved: bool,
    /// The verbatim quote that settles the axis, or `"not specified"`.
    pub evidence: String,
    /// A concrete question that would settle the axis; empty when resolved.
    #[serde(default)]
    pub question: String,
}

/// The model-proposed axes plus the deterministic guidance score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuidanceAssessment {
    /// Decision axes in the model's order (most consequential first).
    pub axes: Vec<DecisionAxis>,
    /// `resolved / total`, or `1.0` when no axes were found.
    pub score: f32,
}

impl GuidanceAssessment {
    /// The unresolved axes, in assessment order.
    #[must_use]
    pub fn open_axes(&self) -> Vec<&DecisionAxis> {
        self.axes.iter().filter(|axis| !axis.resolved).collect()
    }
}

/// The wire shape the model replies with (the score is computed, never
/// model-reported: the model proposes, the runtime decides).
#[derive(Debug, Deserialize)]
struct AxesEnvelope {
    axes: Vec<DecisionAxis>,
}

/// Deterministic score over the model-proposed axes: resolved ÷ total, `1.0`
/// when the list is empty (a trivial idea is not penalized for having nothing
/// to decide).
#[must_use]
pub fn guidance_score(axes: &[DecisionAxis]) -> f32 {
    if axes.is_empty() {
        return 1.0;
    }
    let resolved = axes.iter().filter(|axis| axis.resolved).count();
    #[allow(clippy::cast_precision_loss)] // axis counts are tiny
    {
        resolved as f32 / axes.len() as f32
    }
}

/// Assess how much load-bearing guidance `idea` contains.
///
/// One bounded model call on the same validate-and-retry ladder as intake:
/// invalid JSON is fed back with the parse error, up to the shared attempt
/// cap.
///
/// # Errors
/// Returns [`HarnessError::Provider`] if the provider fails or never produces
/// a parseable assessment within the retry cap.
pub async fn assess_guidance(
    provider: &dyn ModelProvider,
    model: &str,
    idea: &str,
) -> Result<GuidanceAssessment, HarnessError> {
    let seed = vec![
        Message::text(Role::System, GUIDANCE_PROMPT),
        Message::text(Role::User, idea),
    ];
    let axes = generate(provider, model, seed, "guidance assessment", parse_axes).await?;
    let score = guidance_score(&axes);
    Ok(GuidanceAssessment { axes, score })
}

/// Parse the model's reply into decision axes. Tolerates a fenced code block
/// around the JSON (a common local-model habit); everything else must be the
/// exact envelope shape.
fn parse_axes(text: &str) -> Result<Vec<DecisionAxis>, HarnessError> {
    let body = strip_code_fence(text.trim());
    let envelope: AxesEnvelope =
        serde_json::from_str(body).map_err(|err| HarnessError::Malformed {
            document: "guidance assessment",
            detail: format!("expected a JSON object with an \"axes\" array: {err}"),
        })?;
    Ok(envelope.axes)
}

/// Strip one surrounding Markdown code fence (with an optional language tag),
/// returning the inner body; text without a fence is returned unchanged.
fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let Some(stripped) = rest
        .split_once('\n')
        .map(|(_, body)| body)
        .and_then(|body| body.trim_end().strip_suffix("```"))
    else {
        return text;
    };
    stripped.trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_llm::FakeProvider;

    fn axis(name: &str, resolved: bool) -> DecisionAxis {
        DecisionAxis {
            axis: name.to_string(),
            resolved,
            evidence: if resolved {
                "quoted words".to_string()
            } else {
                "not specified".to_string()
            },
            question: if resolved {
                String::new()
            } else {
                format!("what about {name}?")
            },
        }
    }

    #[test]
    fn guidance_prompt_is_stable() {
        insta::assert_snapshot!(GUIDANCE_PROMPT);
    }

    #[test]
    fn score_is_the_resolved_fraction() {
        let axes = vec![axis("a", true), axis("b", false), axis("c", true)];
        let score = guidance_score(&axes);
        assert!((score - 2.0 / 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn score_of_no_axes_is_one() {
        assert!((guidance_score(&[]) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn score_extremes_are_zero_and_one() {
        let none = vec![axis("a", false), axis("b", false)];
        assert!(guidance_score(&none).abs() < f32::EPSILON);
        let all = vec![axis("a", true), axis("b", true)];
        assert!((guidance_score(&all) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn open_axes_preserves_order_and_filters_resolved() {
        let assessment = GuidanceAssessment {
            axes: vec![axis("first", false), axis("mid", true), axis("last", false)],
            score: 1.0 / 3.0,
        };
        let open: Vec<&str> = assessment
            .open_axes()
            .iter()
            .map(|a| a.axis.as_str())
            .collect();
        assert_eq!(open, vec!["first", "last"]);
    }

    #[test]
    fn parse_accepts_bare_and_fenced_json() {
        let json = r#"{"axes":[{"axis":"camera","resolved":false,"evidence":"not specified","question":"fixed or free camera?"}]}"#;
        let bare = parse_axes(json).unwrap();
        assert_eq!(bare.len(), 1);
        assert!(!bare[0].resolved);

        let fenced = format!("```json\n{json}\n```");
        let parsed = parse_axes(&fenced).unwrap();
        assert_eq!(parsed, bare);
    }

    #[test]
    fn parse_rejects_prose_with_a_named_error() {
        let err = parse_axes("here are my thoughts on the idea").unwrap_err();
        assert!(matches!(
            err,
            HarnessError::Malformed { document, .. } if document == "guidance assessment"
        ));
    }

    #[test]
    fn assessment_round_trips_through_serde() {
        let assessment = GuidanceAssessment {
            axes: vec![axis("scope", true), axis("platform", false)],
            score: 0.5,
        };
        let json = serde_json::to_string(&assessment).unwrap();
        let back: GuidanceAssessment = serde_json::from_str(&json).unwrap();
        assert_eq!(back, assessment);
    }

    #[tokio::test]
    async fn assess_scores_a_scripted_reply() {
        let reply = r#"{"axes":[
            {"axis":"scope","resolved":true,"evidence":"only the parser","question":""},
            {"axis":"output format","resolved":false,"evidence":"not specified","question":"JSON or plain text output?"}
        ]}"#;
        let provider = FakeProvider::new().text(reply);
        let assessment = assess_guidance(&provider, "m", "rewrite only the parser")
            .await
            .unwrap();
        assert_eq!(assessment.axes.len(), 2);
        assert!((assessment.score - 0.5).abs() < f32::EPSILON);
        assert_eq!(assessment.open_axes().len(), 1);
    }

    #[tokio::test]
    async fn malformed_reply_is_retried_with_the_parse_error_fed_back() {
        let valid = r#"{"axes":[{"axis":"scope","resolved":true,"evidence":"the whole app","question":""}]}"#;
        let provider = FakeProvider::new().text("not json at all").text(valid);
        let assessment = assess_guidance(&provider, "m", "an idea").await.unwrap();
        assert!((assessment.score - 1.0).abs() < f32::EPSILON);

        // The retry turn carries the parse failure back to the model.
        let requests = provider.requests();
        let retry = requests.last().expect("a retry request");
        let retry_text = retry
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            retry_text.contains("was not valid"),
            "retry did not feed the error back: {retry_text}"
        );
    }

    #[tokio::test]
    async fn exhausted_retries_surface_a_provider_error() {
        let provider = FakeProvider::new().text("no").text("still no").text("nope");
        let err = assess_guidance(&provider, "m", "an idea")
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::Provider(_)));
    }
}
