//! What the Action writes: the sticky comment (found again by its marker) and the check annotations.

use std::fmt::Write;

use crate::policy::Level;
use crate::verdict::{Source, Verdict};

pub const MARKER: &str = "<!-- pr-guard -->";

const LOGO_LIGHT: &str = "https://typestrict.ai/status/logo-light.png";
const LOGO_DARK: &str = "https://typestrict.ai/status/logo-dark.png";
const SITE: &str = "https://typestrict.ai";
const REPO: &str = "https://github.com/typestrict-lab/pr-guard";

/// The sticky comment as GitHub and GitLab render it (HTML for the logo in both themes and the folded details).
pub fn comment(v: &Verdict) -> String {
    render(v, true)
}

/// The same for Bitbucket, whose markdown shows HTML as text: plain markdown, the logo as a link.
pub fn comment_markdown_only(v: &Verdict) -> String {
    render(v, false)
}

fn level_icon(l: Level) -> &'static str {
    match l {
        Level::Danger => "🔴",
        Level::Warning => "🟡",
        Level::Info => "🔵",
    }
}

/// The headline: what the pull request needs, before anything else.
fn headline(v: &Verdict) -> String {
    if v.skipped {
        return "⏭️ Skipped by label".into();
    }
    if v.blocked {
        let why = v.findings.iter().find(|f| f.level == Level::Danger).or(v.findings.first()).map(|f| f.message.as_str()).unwrap_or("policy");
        return format!("🔴 Blocked: {why}");
    }
    match v.reviewers {
        0 if v.auto_merge => "🟢 Ready to merge on its own".into(),
        0 => "🟢 No reviewer needed".into(),
        1 => "🟡 Needs 1 reviewer".into(),
        n => format!("🟠 Needs {n} reviewers"),
    }
}

/// The questions in the order a reviewer reads them, as asked, and whether "yes" is the safe answer.
const QUESTIONS: [(&str, &str, bool); 6] = [
    ("description_matches", "Does it do what the description says?", true),
    ("vulnerability", "Does it introduce a vulnerability?", false),
    ("breaking", "Does it break existing users?", false),
    ("tests_cover", "Do the tests cover it?", true),
    ("resolves_issue", "Does it resolve the linked issue?", true),
    ("focused", "Does it stay on one concern?", true),
];

/// One row per answer: the question, the answer with how sure the model is, and the color of the finding the policy
/// raised for it (green when none).
fn scorecard(v: &Verdict) -> Vec<(&'static str, String, String)> {
    let key = |q: &crate::policy::Question| serde_json::to_value(q).ok().and_then(|x| x.as_str().map(str::to_string)).unwrap_or_default();
    let scores: Vec<(String, f64)> = v.scores.iter().map(|(q, s)| (key(q), *s)).collect();
    let score = |rule: &str| scores.iter().find(|(k, _)| k == rule).map(|(_, s)| *s);
    let icon = |rule: &str| v.findings.iter().filter(|f| f.rule == rule).map(|f| f.level).max().map_or("✅", level_icon);
    let mut rows = Vec::new();
    for (rule, question, yes_is_safe) in QUESTIONS {
        let Some(safe) = score(rule) else { continue };
        let p_yes = if yes_is_safe { safe } else { 1.0 - safe };
        let (answer, sure) = if p_yes >= 0.5 { ("Yes", p_yes) } else { ("No", 1.0 - p_yes) };
        let cwe = match &v.cwe {
            Some(cwe) if rule == "vulnerability" && answer == "Yes" => format!(", likely {cwe}"),
            _ => String::new(),
        };
        rows.push((icon(rule), question.to_string(), format!("{answer} · {:.0}%{cwe}", sure * 100.0)));
    }
    if v.tests_applicable == Some(false) {
        rows.push(("➖", "Do the tests cover it?".into(), "Not applicable: no production code changed".into()));
    }
    if let Some(safe) = score("complexity") {
        const BANDS: [&str; 5] = ["Trivial", "Small", "Medium", "Large", "Very large"];
        // The size informs, it does not approve: neutral from Medium up unless the policy warns about it.
        let band = ((1.0 - safe) * 4.0).round().clamp(0.0, 4.0) as usize;
        let has_finding = v.findings.iter().any(|f| f.rule == "complexity");
        rows.push((if has_finding || band < 2 { icon("complexity") } else { "⚪" }, "How big is it to review?".into(), BANDS[band].into()));
    }
    rows
}

fn render(v: &Verdict, html: bool) -> String {
    let mut s = format!("{MARKER}\n### {}\n\n", headline(v));
    if v.skipped {
        let _ = writeln!(s, "The pull request carries the skip label; nothing was checked.");
        return footer(s, html);
    }
    let mut meta = Vec::new();
    if v.shadow {
        meta.push("**Shadow mode**, nothing is enforced".to_string());
    }
    if let Some(score) = v.score {
        meta.push(format!("score **{:.0}%**", score * 100.0));
    }
    if let Some(t) = &v.change_type {
        let by_rule = if v.change_type_source == Some(Source::Rule) { " (by rule)" } else { "" };
        meta.push(format!("type `{t}`{by_rule}"));
    }
    if !meta.is_empty() {
        let _ = writeln!(s, "{}\n", meta.join(" · "));
    }

    let rows = scorecard(v);
    if !rows.is_empty() {
        s.push_str("| | Question | Answer |\n|:-:|---|---|\n");
        for (icon, question, answer) in rows {
            let _ = writeln!(s, "| {icon} | {question} | {answer} |");
        }
        s.push('\n');
    }
    if !v.not_evaluated.is_empty() {
        let _ = writeln!(s, "⚪ Not evaluated (outside the model's languages): {}\n", v.not_evaluated.join(", "));
    }
    let asked = |rule: &str| rule == "complexity" || QUESTIONS.iter().any(|(r, _, _)| *r == rule);
    let findings: Vec<_> = v.findings.iter().filter(|f| !asked(&f.rule)).collect();
    if !findings.is_empty() {
        s.push_str("**Findings**\n\n| | Finding | Where |\n|:-:|---|---|\n");
        for f in findings {
            let at = match (&f.path, f.line) {
                (Some(p), Some(l)) => format!("`{p}:{l}`"),
                (Some(p), None) => format!("`{p}`"),
                _ => String::new(),
            };
            // The Where column has the path: the message does not repeat it.
            let message = f.path.as_ref().and_then(|p| f.message.strip_suffix(&format!(": {p}"))).unwrap_or(&f.message);
            let _ = writeln!(s, "| {} | {} | {at} |", level_icon(f.level), message.replace('|', "\\|"));
        }
        s.push('\n');
    }
    if let (true, Some(score)) = (html, v.score) {
        s.push_str("<details><summary>How pr-guard decided</summary>\n\n");
        s.push_str(&audit(v, score));
        s.push_str("</details>\n\n");
    }
    footer(s, html)
}

/// The score's arithmetic: each answer as a score on the safe side times its weight, the total, and where it falls
/// between the policy's thresholds; then what raised the count above that.
fn audit(v: &Verdict, score: f64) -> String {
    let mut s = String::from("| Question | Score (1 = safe) | Weight | Points |\n|---|--:|--:|--:|\n");
    let names: Vec<(&str, &str)> = QUESTIONS.iter().map(|(r, q, _)| (*r, *q)).chain([("complexity", "How big is it to review?")]).collect();
    let (mut points, mut total) = (0.0, 0.0);
    // In the order of the questions table above.
    let rule_of = |q: &crate::policy::Question| serde_json::to_value(q).ok().and_then(|x| x.as_str().map(str::to_string)).unwrap_or_default();
    let mut rows: Vec<_> = v.weights.iter().filter_map(|(q, w)| Some((rule_of(q), *w, *v.scores.get(q)?))).collect();
    rows.sort_by_key(|(rule, _, _)| names.iter().position(|(r, _)| r == rule).unwrap_or(usize::MAX));
    for (rule, w, sc) in rows {
        let name = names.iter().find(|(r, _)| *r == rule).map_or(rule.as_str(), |(_, n)| n);
        let _ = writeln!(s, "| {name} | {sc:.2} | {w} | {:.2} |", sc * w);
        points += sc * w;
        total += w;
    }
    let _ = writeln!(s, "| **Score** | | **{total}** | **{points:.2} / {total} = {:.0}%** |\n", score * 100.0);
    if let Some(t) = v.thresholds {
        let band = if score >= t.none_at { 0 } else if score >= t.one_at { 1 } else { 2 };
        let _ = write!(
            s,
            "{:.0}% or more needs no reviewer, {:.0}% or more one, below that two: **{}** by the score.",
            t.none_at * 100.0,
            t.one_at * 100.0,
            match band { 0 => "no reviewer", 1 => "1 reviewer", _ => "2 reviewers" }
        );
        if v.reviewers > band {
            let why = if v.findings.iter().any(|f| f.level == Level::Danger) {
                "a danger finding asks for two"
            } else if !v.not_evaluated.is_empty() {
                "code outside the model's languages asks for at least one"
            } else {
                "a warning asks for at least one"
            };
            let _ = write!(s, " Raised to **{}**: {why}.", if v.reviewers == 1 { "1 reviewer".into() } else { format!("{} reviewers", v.reviewers) });
        }
        s.push('\n');
    }
    s.push_str("\nThe thresholds and weights are the policy's (`.github/pr-guard.yml`; `pr-guard init` prints the defaults).\n");
    s
}

/// The signature: where it ran and who made it. With HTML, the typestrict.ai wordmark in the reader's theme.
fn footer(mut s: String, html: bool) -> String {
    let local = format!("Checked on this runner by [pr-guard]({REPO}) with TS-PRGuard; the code never left it.");
    if html {
        let _ = write!(
            s,
            "<sub>{local}</sub>\n\n<p align=\"right\"><sub>powered by</sub>&nbsp;<a href=\"{SITE}\"><picture>\
             <source media=\"(prefers-color-scheme: dark)\" srcset=\"{LOGO_DARK}\">\
             <img alt=\"typestrict.ai\" src=\"{LOGO_LIGHT}\" height=\"14\" align=\"absmiddle\"></picture></a></p>\n"
        );
    } else {
        let _ = writeln!(s, "---\n{local} Powered by [typestrict.ai]({SITE}).");
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
        assert!(c.starts_with(MARKER) && c.contains("### 🟠 Needs 2 reviewers") && c.contains("| 🔴 | Looks like a GitHub token"));
        assert!(c.contains("| ✅ | Does it do what the description says? | Yes · 90% |") && c.contains("prefers-color-scheme: dark") && c.contains(LOGO_LIGHT));
        let sensitive = Finding::at(Level::Warning, "sensitive_paths", "Changes a sensitive path: src/auth.ts".into(), "src/auth.ts", None);
        let big = Answers { complexity: Some(3), ..a.clone() };
        let c2 = comment(&decide(&Policy::default(), &files, &big, &[], vec![sensitive]));
        assert!(c2.contains("| **Score** |") && c2.contains("one, below that two: **1 reviewer** by the score."), "{c2}");
        // A danger raises the count past the score's band, and the accordion says why.
        let leak2 = Finding::at(Level::Danger, "secrets", "Looks like a token".into(), "src/a.ts", Some(1));
        let c3 = comment(&decide(&Policy::default(), &files, &a, &[], vec![leak2]));
        assert!(c3.contains("Raised to **2 reviewers**: a danger finding asks for two."), "{c3}");
        assert!(c2.contains("| 🟡 | Changes a sensitive path | `src/auth.ts` |") && c2.contains("| ⚪ | How big is it to review? | Large |"), "{c2}");
        let plain = comment_markdown_only(&v);
        assert!(!plain.contains('<') || plain.matches('<').count() == 1, "only the marker is HTML on Bitbucket: {plain}");
        assert!(plain.contains("Powered by [typestrict.ai](https://typestrict.ai)"));
        assert_eq!(annotations(&v), "::error file=src/a.ts,line=3,title=pr-guard secrets::Looks like a GitHub token committed in the code\n");
    }
}
