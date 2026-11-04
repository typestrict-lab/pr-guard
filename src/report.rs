//! What the Action writes: the sticky comment (found again by its marker) and the check annotations.

use std::fmt::Write;

use crate::policy::Level;
use crate::verdict::{Source, Verdict};

pub const MARKER: &str = "<!-- pr-guard -->";

pub fn comment(v: &Verdict) -> String {
    let mut s = format!("{MARKER}\n### pr-guard\n\n");
    if v.skipped {
        s.push_str("Skipped by label.\n");
        return s;
    }
    let who = match v.reviewers {
        0 if v.auto_merge => "No reviewer needed: labeled for auto-merge.".to_string(),
        0 => "No reviewer needed.".to_string(),
        1 => "Needs **1 reviewer**.".to_string(),
        n => format!("Needs **{n} reviewers**."),
    };
    let _ = writeln!(s, "{who}{}", if v.shadow { " _(shadow mode: nothing is enforced)_" } else { "" });
    if let Some(score) = v.score {
        let by_rule = if v.change_type_source == Some(Source::Rule) { " (by rule)" } else { "" };
        let _ = writeln!(s, "\nCombined score: **{:.0}%**{}", score * 100.0, v.change_type.as_ref().map(|t| format!(" · type: `{t}`{by_rule}")).unwrap_or_default());
    }
    if v.tests_applicable == Some(false) {
        let _ = writeln!(s, "\nTests: not applicable (no production code changed).");
    }
    if !v.not_evaluated.is_empty() {
        let _ = writeln!(s, "\nNot evaluated (outside the model's languages): {}", v.not_evaluated.join(", "));
    }
    if !v.findings.is_empty() {
        s.push_str("\n| | Finding | Where |\n|---|---|---|\n");
        for f in &v.findings {
            let icon = match f.level {
                Level::Danger => "🔴",
                Level::Warning => "🟡",
                Level::Info => "🔵",
            };
            let at = match (&f.path, f.line) {
                (Some(p), Some(l)) => format!("`{p}:{l}`"),
                (Some(p), None) => format!("`{p}`"),
                _ => String::new(),
            };
            let _ = writeln!(s, "| {icon} | {} | {at} |", f.message.replace('|', "\\|"));
        }
    }
    if !v.scores.is_empty() {
        s.push_str("\n<details><summary>Answers</summary>\n\n| Question | Score |\n|---|---|\n");
        for (q, score) in &v.scores {
            let _ = writeln!(s, "| `{}` | {:.0}% |", serde_json::to_value(q).unwrap().as_str().unwrap_or_default(), score * 100.0);
        }
        s.push_str("\n1 is the safe side; the policy weighs them into the combined score.\n</details>\n");
    }
    s
}

/// GitHub workflow commands: the runner turns each into an annotation on the file and line.
pub fn annotations(v: &Verdict) -> String {
    let esc = |s: &str| s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A");
    let prop = |s: &str| esc(s).replace(':', "%3A").replace(',', "%2C");
    v.findings
        .iter()
        .map(|f| {
            let kind = match f.level {
                Level::Danger => "error",
                Level::Warning => "warning",
                Level::Info => "notice",
            };
            let mut props = Vec::new();
            if let Some(p) = &f.path {
                props.push(format!("file={}", prop(p)));
            }
            if let Some(l) = f.line {
                props.push(format!("line={l}"));
            }
            props.push(format!("title=pr-guard {}", prop(&f.rule)));
            format!("::{kind} {}::{}\n", props.join(","), esc(&f.message))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::FileChange;
    use crate::policy::Policy;
    use crate::verdict::{Answers, Finding, decide};

    #[test]
    fn comment_and_annotations_carry_each_finding() {
        let files = [FileChange { path: "src/a.ts".into(), deleted: false, added: vec![], removed: 0 }];
        let leak = Finding::at(Level::Danger, "secrets", "Looks like a GitHub token committed in the code".into(), "src/a.ts", Some(3));
        let a = Answers { description_matches: Some(0.9), ..Answers::default() };
        let v = decide(&Policy::default(), &files, &a, &[], vec![leak]);
        let c = comment(&v);
        assert!(c.starts_with(MARKER) && c.contains("Needs **2 reviewers**") && c.contains("| 🔴 | Looks like a GitHub token"));
        assert_eq!(annotations(&v), "::error file=src/a.ts,line=3,title=pr-guard secrets::Looks like a GitHub token committed in the code\n");
    }
}
