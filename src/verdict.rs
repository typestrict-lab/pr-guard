//! The decision: the model's answers through the policy's gates and weights, plus the deterministic findings, give the
//! number of reviewers, whether the check blocks, and the labels.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::diff::FileChange;
use crate::lang;
use crate::policy::{Level, Mode, Policy, Question, Reviewers};

/// TS-PRGuard's answers for one pull request. Probabilities are of the question as asked (for `vulnerability`, the chance
/// that the change introduces one); `complexity` is the band, 0 (trivial) to 4 (very complex). Absent = not asked.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answers {
    pub description_matches: Option<f64>,
    pub complexity: Option<u8>,
    pub vulnerability: Option<f64>,
    pub cwe: Option<String>,
    pub tests_cover: Option<f64>,
    pub change_type: Option<String>,
    pub breaking: Option<f64>,
    /// Only when the pull request links an issue.
    pub resolves_issue: Option<f64>,
    pub focused: Option<f64>,
    /// Where `change_type` came from.
    pub change_type_source: Option<Source>,
    /// `Some(false)` when the change calls for no tests (no production code changed); `tests_cover` is then absent
    /// when a rule said so.
    pub tests_applicable: Option<bool>,
    /// Where the tests answer came from.
    pub tests_source: Option<Source>,
}

/// Who answered: a rule over the changed files, or the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Rule,
    Model,
}

impl Answers {
    /// Each answer as a score where 1 is the safe side.
    pub fn scores(&self) -> BTreeMap<Question, f64> {
        let inv = |p: Option<f64>| p.map(|p| 1.0 - p);
        [
            (Question::DescriptionMatches, self.description_matches),
            (Question::Complexity, self.complexity.map(|b| 1.0 - f64::from(b.min(4)) / 4.0)),
            (Question::Vulnerability, inv(self.vulnerability)),
            (Question::TestsCover, self.tests_cover),
            (Question::Breaking, inv(self.breaking)),
            (Question::ResolvesIssue, self.resolves_issue),
            (Question::Focused, self.focused),
        ]
        .into_iter()
        .filter_map(|(q, s)| Some((q, s?.clamp(0.0, 1.0))))
        .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub level: Level,
    pub rule: String,
    pub message: String,
    pub path: Option<String>,
    pub line: Option<u32>,
}

impl Finding {
    pub fn new(level: Level, rule: &str, message: String) -> Self {
        Finding { level, rule: rule.into(), message, path: None, line: None }
    }

    pub fn at(level: Level, rule: &str, message: String, path: &str, line: Option<u32>) -> Self {
        Finding { path: Some(path.into()), line, ..Finding::new(level, rule, message) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub skipped: bool,
    pub shadow: bool,
    /// Weighted mean of the scores the policy weighs; `None` when nothing was scored.
    pub score: Option<f64>,
    pub scores: BTreeMap<Question, f64>,
    pub reviewers: u8,
    pub auto_merge: bool,
    pub blocked: bool,
    /// Changed code in languages the model was not evaluated on.
    pub not_evaluated: Vec<String>,
    pub findings: Vec<Finding>,
    pub change_type: Option<String>,
    pub cwe: Option<String>,
    pub labels: Vec<String>,
    #[serde(default)]
    pub change_type_source: Option<Source>,
    #[serde(default)]
    pub tests_applicable: Option<bool>,
    #[serde(default)]
    pub tests_source: Option<Source>,
    /// How the score was made, for the comment: the weight of each answer the policy weighed, and its reviewer
    /// thresholds.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub weights: BTreeMap<Question, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<Reviewers>,
}

pub fn decide(policy: &Policy, files: &[FileChange], answers: &Answers, pr_labels: &[String], rule_findings: Vec<Finding>) -> Verdict {
    let shadow = policy.mode == Mode::Shadow;
    if pr_labels.contains(&policy.skip_label) {
        return Verdict {
            skipped: true, shadow, score: None, scores: BTreeMap::new(), reviewers: 0, auto_merge: false, blocked: false,
            not_evaluated: vec![], findings: vec![], change_type: None, cwe: None, labels: vec![], change_type_source: None,
            tests_applicable: None, tests_source: None, weights: BTreeMap::new(), thresholds: None,
        };
    }
    let mut not_evaluated: Vec<String> = files
        .iter()
        .filter(|f| !f.deleted)
        .filter_map(|f| lang::of(&f.path))
        .filter(|l| !policy.languages.iter().any(|s| s == l))
        .map(String::from)
        .collect();
    not_evaluated.sort();
    not_evaluated.dedup();
    let has_code = files.iter().any(|f| !f.deleted && lang::of(&f.path).is_some());

    let scores = answers.scores();
    let mut findings = rule_findings;
    let (mut sum, mut weights, mut weighed) = (0.0, 0.0, BTreeMap::new());
    for (q, gate) in &policy.questions {
        let Some(&s) = scores.get(q) else { continue };
        sum += gate.weight * s;
        weights += gate.weight;
        weighed.insert(*q, gate.weight);
        let level = if gate.min.is_some_and(|m| s < m) {
            Some(Level::Danger)
        } else if gate.warn_below.is_some_and(|w| s < w) {
            Some(Level::Warning)
        } else {
            None
        };
        if let Some(level) = level {
            findings.push(Finding::new(level, question_rule(*q), question_message(*q, answers)));
        }
    }
    let score = (weights > 0.0).then(|| sum / weights);
    let worst = findings.iter().map(|f| f.level).max();

    let mut reviewers = match score {
        Some(s) if s >= policy.reviewers.none_at => 0,
        Some(s) if s >= policy.reviewers.one_at => 1,
        Some(_) => 2,
        // Nothing scored: code changes wait for two reviewers; a change with no code (docs, config) needs none by itself.
        None => if has_code { 2 } else { 0 },
    };
    if !not_evaluated.is_empty() || worst == Some(Level::Warning) {
        reviewers = reviewers.max(1);
    }
    if worst == Some(Level::Danger) {
        reviewers = 2;
    }
    let auto_merge = policy.auto_merge && !shadow && reviewers == 0 && not_evaluated.is_empty() && worst.is_none_or(|l| l == Level::Info);
    let blocked = !shadow && policy.block_on.is_some_and(|b| worst.is_some_and(|w| w >= b));

    let mut labels = vec![format!("pr-guard:reviewers-{reviewers}")];
    if auto_merge {
        labels.push("pr-guard:auto-merge".into());
    }
    if let Some(t) = &answers.change_type {
        labels.push(format!("pr-guard:type-{t}"));
    }
    if !not_evaluated.is_empty() {
        labels.push("pr-guard:not-evaluated".into());
    }
    findings.sort_by_key(|f| std::cmp::Reverse(f.level));
    Verdict {
        skipped: false, shadow, score, scores, reviewers, auto_merge, blocked, not_evaluated, findings,
        change_type: answers.change_type.clone(), cwe: answers.cwe.clone(), labels, change_type_source: answers.change_type_source,
        tests_applicable: answers.tests_applicable, tests_source: answers.tests_source, weights: weighed,
        thresholds: Some(policy.reviewers),
    }
}

fn question_rule(q: Question) -> &'static str {
    match q {
        Question::DescriptionMatches => "description_matches",
        Question::Complexity => "complexity",
        Question::Vulnerability => "vulnerability",
        Question::TestsCover => "tests_cover",
        Question::Breaking => "breaking",
        Question::ResolvesIssue => "resolves_issue",
        Question::Focused => "focused",
    }
}

fn question_message(q: Question, a: &Answers) -> String {
    let pct = |p: Option<f64>| format!("{:.0}%", p.unwrap_or(0.0) * 100.0);
    match q {
        Question::DescriptionMatches => format!("The description may not match the diff ({} it does)", pct(a.description_matches)),
        Question::Complexity => format!("Complex change (band {} of 4)", a.complexity.unwrap_or(0)),
        Question::Vulnerability => match &a.cwe {
            Some(cwe) => format!("May introduce a vulnerability ({}, likely {cwe})", pct(a.vulnerability)),
            None => format!("May introduce a vulnerability ({})", pct(a.vulnerability)),
        },
        Question::TestsCover => format!("The tests may not cover the change ({} they do)", pct(a.tests_cover)),
        Question::Breaking => format!("May break compatibility ({})", pct(a.breaking)),
        Question::ResolvesIssue => format!("May not resolve the linked issue ({} it does)", pct(a.resolves_issue)),
        Question::Focused => format!("The change may mix unrelated work ({} focused)", pct(a.focused)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default policy enforcing, which these tests decide under (the default itself is shadow).
    fn enforce() -> Policy {
        Policy { mode: Mode::Enforce, ..Policy::default() }
    }

    fn code(path: &str) -> FileChange {
        FileChange { path: path.into(), deleted: false, added: vec![(1, "x".into())], removed: 0 }
    }

    fn good() -> Answers {
        Answers {
            description_matches: Some(0.95), complexity: Some(1), vulnerability: Some(0.02), tests_cover: Some(0.9),
            change_type: Some("fix".into()), breaking: Some(0.05), focused: Some(0.9), ..Answers::default()
        }
    }

    #[test]
    fn a_clean_change_needs_no_reviewer_and_auto_merges_only_when_the_policy_allows() {
        let files = [code("src/a.ts")];
        let v = decide(&enforce(), &files, &good(), &[], vec![]);
        assert_eq!((v.reviewers, v.auto_merge, v.blocked), (0, false, false));
        assert!(v.score.unwrap() >= 0.85);
        let p = Policy { auto_merge: true, ..enforce() };
        let v = decide(&p, &files, &good(), &[], vec![]);
        assert!(v.auto_merge);
        assert_eq!(v.labels, ["pr-guard:reviewers-0", "pr-guard:auto-merge", "pr-guard:type-fix"]);
    }

    #[test]
    fn a_likely_vulnerability_warns_by_default_and_blocks_where_the_policy_makes_it_a_danger_unless_shadow() {
        let a = Answers { vulnerability: Some(0.8), cwe: Some("CWE-89".into()), ..good() };
        let v = decide(&enforce(), &[code("src/db.py")], &a, &[], vec![]);
        assert_eq!((v.findings[0].level, v.blocked), (Level::Warning, false));
        assert!(v.reviewers >= 1);
        assert_eq!(v.findings[0].message, "May introduce a vulnerability (80%, likely CWE-89)");
        let mut strict = enforce();
        strict.questions.get_mut(&Question::Vulnerability).unwrap().min = Some(0.4);
        let v = decide(&strict, &[code("src/db.py")], &a, &[], vec![]);
        assert_eq!((v.reviewers, v.blocked), (2, true));
        let shadow = Policy { mode: Mode::Shadow, auto_merge: true, ..strict };
        let v = decide(&shadow, &[code("src/db.py")], &a, &[], vec![]);
        assert_eq!((v.reviewers, v.blocked, v.auto_merge), (2, false, false));
    }

    #[test]
    fn uncovered_languages_and_warnings_ask_for_at_least_one_reviewer_and_never_auto_merge() {
        let p = Policy { auto_merge: true, ..enforce() };
        let v = decide(&p, &[code("src/a.ts"), code("native/b.cpp")], &good(), &[], vec![]);
        assert_eq!((v.reviewers, v.auto_merge), (1, false));
        assert_eq!(v.not_evaluated, ["c++"]);
        let warn = Finding::new(Level::Warning, "size", "big".into());
        let v = decide(&p, &[code("src/a.ts")], &good(), &[], vec![warn]);
        assert_eq!((v.reviewers, v.auto_merge, v.blocked), (1, false, false));
    }

    #[test]
    fn the_skip_label_skips_and_a_docs_only_change_with_no_answers_needs_nobody() {
        let v = decide(&enforce(), &[code("src/a.ts")], &good(), &["pr-guard:skip".into()], vec![]);
        assert!(v.skipped && !v.blocked);
        let v = decide(&enforce(), &[code("README.md")], &Answers::default(), &[], vec![]);
        assert_eq!((v.score, v.reviewers), (None, 0));
        let v = decide(&enforce(), &[code("src/a.go")], &Answers::default(), &[], vec![]);
        assert_eq!(v.reviewers, 2);
    }
}
