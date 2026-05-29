//! Canonical "Hard Rules" for AI-agent guidance documents.
//!
//! ## Honest framing
//!
//! These rules are **helpful, not enforced**. An LLM following an instruction
//! is a *probabilistic* behavior, not a hard constraint: even explicit,
//! prominently-placed instructions are followed only some fraction of the time
//! (Geng et al. 2025, "Control Illusion"). We surface them as
//! *defense-in-depth* — front-of-context nudges that raise the odds of
//! correct behavior — not as guarantees. Do not build safety-critical control
//! flow on the assumption that an agent obeyed a rule below.
//!
//! ## Rule scarcity
//!
//! Rules are deliberately FEW. Each additional rule dilutes the scarce
//! front-of-context attention the others compete for, so we cap the canonical
//! set at a handful of high-leverage behaviors. Prefer improving an existing
//! rule over adding a new one.

/// Bump whenever the canonical rule set changes (content, order, or count).
/// Emitted into generated-guide frontmatter as `rules_version` so downstream
/// tooling can detect drift between a checked-in guide and the current binary.
pub const RULES_VERSION: u32 = 1;

/// A single hard rule. `id` is a stable slug for tooling; `title` is a short
/// imperative headline; `body` is the actionable instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub id: &'static str,
    pub title: &'static str,
    pub body: &'static str,
}

/// The canonical, built-in hard-rule set.
///
/// Rule 2 ("enumerate then verify") is evidence-backed: Chain-of-Verification
/// (Dhuliawala et al. 2024) shows that having a model independently verify its
/// own draft answers against retrieved facts substantially reduces
/// hallucination on list/aggregate ("every X" / "all Y") questions.
pub const HARD_RULES: &[Rule] = &[
    Rule {
        id: "close-loopholes",
        title: "Close your own loopholes",
        body: "If your answer references a file path, read that file first. Do not \
               cite, summarize, or reason about a file you have not opened.",
    },
    Rule {
        id: "enumerate-then-verify",
        title: "Enumerate then verify",
        // Evidence: Chain-of-Verification (Dhuliawala et al. 2024) — independent
        // verification of draft answers against retrieved facts cuts
        // hallucination on enumeration/aggregate questions.
        body: "For any \"every X\" / \"all Y\" question, run a regex/grep sweep to \
               enumerate the full set before answering. Verify your draft answer \
               against that sweep rather than trusting recall.",
    },
    Rule {
        id: "project-state-routing",
        title: "Route project-state questions correctly",
        body: "For project-state questions (status, scope, what's in a project), \
               prefer `project_context <slug>` over `brain_search`.",
    },
    Rule {
        id: "fetch-urls",
        title: "Fetch URLs before answering",
        body: "For URL-bearing messages, fetch the URL before answering. Do not \
               answer from prior knowledge of what a link probably contains.",
    },
];

/// Render the hard-rule block as markdown.
///
/// Each rule is prefixed with `**HARD RULE:**` and is intended to sit at the
/// TOP of a generated guide where front-of-context attention is highest.
pub fn render_rules_markdown(rules: &[Rule]) -> String {
    let mut out = String::new();
    out.push_str("## Hard Rules\n\n");
    out.push_str(
        "> These rules are helpful, not enforced — instruction-following by an LLM is \
         probabilistic (Geng et al. 2025). They are defense-in-depth.\n\n",
    );
    for rule in rules {
        out.push_str(&format!(
            "**HARD RULE:** {} — {}\n\n",
            rule.title, rule.body
        ));
    }
    out
}

/// Parse an override rule set from a TOML or markdown file.
///
/// TOML form (detected by a `[[rules]]` table array):
///
/// ```toml
/// [[rules]]
/// id = "my-rule"
/// title = "Do the thing"
/// body = "Always do the thing before the other thing."
/// ```
///
/// Markdown form (any other input): every non-empty, non-heading line becomes
/// a rule body. A leading `**HARD RULE:**` prefix and a `Title — body` split
/// are both honored.
pub fn parse_rules_override(contents: &str) -> Result<Vec<OwnedRule>, anyhow::Error> {
    if contents.contains("[[rules]]") {
        #[derive(serde::Deserialize)]
        struct RuleFile {
            rules: Vec<OwnedRule>,
        }
        let parsed: RuleFile = toml::from_str(contents)?;
        return Ok(parsed.rules);
    }

    // Markdown / plain-text fallback.
    let mut rules = Vec::new();
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('>') {
            continue;
        }
        let line = line
            .trim_start_matches("**HARD RULE:**")
            .trim_start_matches("- ")
            .trim();
        if line.is_empty() {
            continue;
        }
        let (title, body) = match line.split_once(" — ").or_else(|| line.split_once(" - ")) {
            Some((t, b)) => (t.trim().to_string(), b.trim().to_string()),
            None => (String::new(), line.to_string()),
        };
        rules.push(OwnedRule {
            id: format!("custom-{}", rules.len() + 1),
            title,
            body,
        });
    }
    Ok(rules)
}

/// Owned counterpart to [`Rule`], used for runtime-loaded overrides.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct OwnedRule {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub body: String,
}

/// Render an owned override rule set as markdown (same shape as built-ins).
pub fn render_owned_rules_markdown(rules: &[OwnedRule]) -> String {
    let mut out = String::new();
    out.push_str("## Hard Rules\n\n");
    out.push_str(
        "> These rules are helpful, not enforced — instruction-following by an LLM is \
         probabilistic (Geng et al. 2025). They are defense-in-depth.\n\n",
    );
    for rule in rules {
        if rule.title.is_empty() {
            out.push_str(&format!("**HARD RULE:** {}\n\n", rule.body));
        } else {
            out.push_str(&format!(
                "**HARD RULE:** {} — {}\n\n",
                rule.title, rule.body
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_rules_render_with_prefix() {
        let md = render_rules_markdown(HARD_RULES);
        assert!(md.contains("## Hard Rules"));
        assert!(md.contains("**HARD RULE:**"));
        assert!(md.contains("Close your own loopholes"));
        assert!(md.contains("enforced")); // honest framing present
    }

    #[test]
    fn rules_are_few() {
        // Guard against rule-bloat: keep the canonical set small.
        assert!(HARD_RULES.len() <= 6, "rule set should stay small");
    }

    #[test]
    fn parse_toml_override() {
        let toml = r#"
[[rules]]
id = "only-rule"
title = "Be brief"
body = "Answer in one sentence."
"#;
        let rules = parse_rules_override(toml).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "only-rule");
        assert_eq!(rules[0].title, "Be brief");
    }

    #[test]
    fn parse_markdown_override() {
        let md =
            "# My rules\n\n- Always read the file — open it before citing.\n- Fetch the URL.\n";
        let rules = parse_rules_override(md).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].title, "Always read the file");
        assert_eq!(rules[1].body, "Fetch the URL.");
    }
}
