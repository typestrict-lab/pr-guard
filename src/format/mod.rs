//! The verdict in each output format: text for a person at a terminal, JSON for scripts, markdown for the sticky
//! comment, and SARIF, Code Climate and JUnit so most CIs show the result without an integration of their own.

mod codeclimate;
mod junit;
mod sarif;

use std::fmt::Write;

use sha2::{Digest, Sha256};

use crate::policy::{Level, Question};
use crate::report;
use crate::verdict::{Finding, Source, Verdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Human summary (colored on a terminal).
    Text,
    /// The verdict as JSON.
    Json,
    /// The sticky comment.
    Markdown,
    /// SARIF 2.1.0 (GitHub code scanning, Azure DevOps).
    Sarif,
    /// GitLab Code Quality.
    Codeclimate,
    /// JUnit XML (Jenkins, CircleCI, Bitrise, most CIs).
    Junit,
}

pub fn render(format: Format, v: &Verdict, color: bool) -> String {
    match format {
        Format::Text => text(v, color),
        Format::Json => format!("{}\n", serde_json::to_string(v).expect("a verdict always serializes")),
        Format::Markdown => report::comment(v),
        Format::Sarif => sarif::render(v),
        Format::Codeclimate => codeclimate::render(v),
        Format::Junit => junit::render(v),
    }
}

/// Colors only for a terminal that wants them (https://no-color.org: any non-empty `NO_COLOR` turns them off).
pub fn use_color(is_terminal: bool, no_color: Option<&str>) -> bool {
    is_terminal && no_color.is_none_or(str::is_empty)
}

fn question_name(q: &Question) -> String {
    serde_json::to_value(q).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

/// Stable across runs of the same finding, for the tools that track a finding from one run to the next.
fn fingerprint(f: &Finding) -> String {
    let line = f.line.map(|l| l.to_string()).unwrap_or_default();
    let digest = Sha256::digest([f.rule.as_str(), f.path.as_deref().unwrap_or(""), &line, &f.message].join("\0"));
    digest.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn location(f: &Finding) -> String {
    match (&f.path, f.line) {
        (Some(p), Some(l)) => format!("{p}:{l}"),
        (Some(p), None) => p.clone(),
        _ => String::new(),
    }
}

fn text(v: &Verdict, color: bool) -> String {
    let paint = |code: &str, s: &str| if color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() };
    let mut s = String::new();
    if v.skipped {
        let _ = writeln!(s, "pr-guard: {}", paint("2", "skipped by label"));
        return s;
    }
    let status = if v.blocked { paint("1;31", "blocked") } else { paint("1;32", "passed") };
    let who = match v.reviewers {
        0 if v.auto_merge => "no reviewer needed, labeled for auto-merge".to_string(),
        0 => "no reviewer needed".to_string(),
        1 => "needs 1 reviewer".to_string(),
        n => format!("needs {n} reviewers"),
    };
    let shadow = if v.shadow { paint("2", " (shadow mode: nothing is enforced)") } else { String::new() };
    let _ = writeln!(s, "pr-guard: {status}, {who}{shadow}");
    let mut facts = Vec::new();
    if let Some(score) = v.score {
        facts.push(format!("score {:.0}%", score * 100.0));
    }
    if let Some(t) = &v.change_type {
        facts.push(format!("type {t}{}", if v.change_type_source == Some(Source::Rule) { " (by rule)" } else { "" }));
    }
    if v.tests_applicable == Some(false) {
        facts.push(format!("tests not applicable{}", if v.tests_source == Some(Source::Rule) { " (no code changed, by rule)" } else { "" }));
    }
    if !facts.is_empty() {
        let _ = writeln!(s, "{}", facts.join(", "));
    }
    if !v.not_evaluated.is_empty() {
        let _ = writeln!(s, "not evaluated (outside the model's languages): {}", v.not_evaluated.join(", "));
    }
    if !v.findings.is_empty() {
        s.push('\n');
        for f in &v.findings {
            let (code, name) = match f.level {
                Level::Danger => ("31", "danger "),
                Level::Warning => ("33", "warning"),
                Level::Info => ("34", "info   "),
            };
            let at = location(f);
            let at = if at.is_empty() { String::new() } else { format!("{at}  ") };
            let _ = writeln!(s, "  {}  {at}{} {}", paint(code, name), f.message, paint("2", &format!("[{}]", f.rule)));
        }
    }
    if !v.scores.is_empty() {
        let answers: Vec<String> = v.scores.iter().map(|(q, score)| format!("{} {:.0}%", question_name(q), score * 100.0)).collect();
        let _ = writeln!(s, "\n{} {}", paint("2", "answers (1 is the safe side):"), answers.join(", "));
    }
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::diff::FileChange;
    use crate::policy::{Mode, Policy};
    use crate::verdict::{Answers, decide};

    /// A blocked verdict with a located rule finding, an unlocated one and a question finding.
    pub(crate) fn sample(mode: Mode) -> Verdict {
        let files = [FileChange { path: "src/a.ts".into(), deleted: false, added: vec![], removed: 0 }];
        let found = vec![
            Finding::at(Level::Danger, "secrets", "Looks like a GitHub token committed in the code".into(), "src/a.ts", Some(3)),
            Finding::new(Level::Warning, "size", "900 changed lines, above the 800 the policy allows".into()),
        ];
        let a = Answers { description_matches: Some(0.9), tests_cover: Some(0.2), change_type: Some("fix".into()), ..Answers::default() };
        decide(&Policy { mode, ..Policy::default() }, &files, &a, &[], found)
    }

    #[test]
    fn text_is_plain_without_color_and_names_every_finding() {
        let t = text(&sample(Mode::Enforce), false);
        assert!(!t.contains('\x1b'));
        assert!(t.starts_with("pr-guard: blocked, needs 2 reviewers\n"), "{t}");
        assert!(t.contains("  danger   src/a.ts:3  Looks like a GitHub token committed in the code [secrets]"), "{t}");
        assert!(t.contains("tests_cover 20%"));
        assert!(text(&sample(Mode::Enforce), true).contains("\x1b[1;31mblocked\x1b[0m"));
        assert!(text(&sample(Mode::Shadow), false).starts_with("pr-guard: passed, needs 2 reviewers (shadow mode"));
    }

    #[test]
    fn answers_settled_by_a_rule_say_so_in_text_markdown_and_json() {
        let files = [FileChange { path: "README.md".into(), deleted: false, added: vec![], removed: 0 }];
        let a = Answers {
            description_matches: Some(0.9), change_type: Some("Documentation only".into()), change_type_source: Some(Source::Rule),
            tests_applicable: Some(false), tests_source: Some(Source::Rule), ..Answers::default()
        };
        let v = decide(&Policy::default(), &files, &a, &[], vec![]);
        let t = text(&v, false);
        assert!(t.contains("type Documentation only (by rule), tests not applicable (no code changed, by rule)"), "{t}");
        let m = report::comment(&v);
        assert!(m.contains("type `Documentation only` (by rule)") && m.contains("| ➖ | Do the tests cover it? | Not applicable"), "{m}");
        let j: serde_json::Value = serde_json::from_str(&render(Format::Json, &v, false)).unwrap();
        assert_eq!((j["change_type"].as_str(), j["change_type_source"].as_str(), j["tests_applicable"].as_bool()), (Some("Documentation only"), Some("rule"), Some(false)));
        // A verdict written before these fields still reads.
        let mut old = j.clone();
        for k in ["change_type_source", "tests_applicable", "tests_source"] {
            old.as_object_mut().unwrap().remove(k);
        }
        assert_eq!(serde_json::from_value::<Verdict>(old).unwrap().change_type_source, None);
    }

    #[test]
    fn no_color_and_pipes_turn_colors_off() {
        assert!(use_color(true, None) && use_color(true, Some("")));
        assert!(!use_color(true, Some("1")) && !use_color(false, None));
    }

    #[test]
    fn json_is_the_verdict_and_fingerprints_are_stable_hex() {
        let v = sample(Mode::Enforce);
        let back: Verdict = serde_json::from_str(&render(Format::Json, &v, false)).unwrap();
        assert_eq!(back, v);
        let fp = fingerprint(&v.findings[0]);
        assert_eq!((fp.len(), fp.chars().all(|c| c.is_ascii_hexdigit())), (64, true));
        assert_eq!(fp, fingerprint(&v.findings[0].clone()));
        assert_ne!(fp, fingerprint(&v.findings[1]));
        assert_eq!(render(Format::Markdown, &v, false), report::comment(&v));
    }
}
