use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use pr_guard::format::{self, Format};
use pr_guard::policy::{Mode, Policy};
use pr_guard::verdict::Source;
use pr_guard::{ci, diff, facts, git, host, model, pull, report, rules, verdict};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ModeArg {
    Shadow,
    Enforce,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once
enum Cmd {
    /// Decide a pull request: the policy over the diff and TS-PRGuard's answers.
    Check {
        /// A unified diff to read instead of git (`-` for stdin).
        #[arg(long, value_name = "FILE", conflicts_with_all = ["staged", "base", "head"])]
        diff: Option<PathBuf>,
        /// Compare the staged changes with HEAD (pre-commit) instead of a commit range.
        #[arg(long, conflicts_with_all = ["base", "head"])]
        staged: bool,
        /// The base of the range; default the PR's target branch in CI, else origin/HEAD, main or master. The range starts
        /// at its merge base with the head.
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// The head of the range; default HEAD.
        #[arg(long, value_name = "REF")]
        head: Option<String>,
        /// TS-PRGuard's answers as JSON, instead of running the model.
        #[arg(long)]
        answers: Option<PathBuf>,
        /// The pull request's description (title and body); default the PR's in CI, else the range's commit messages.
        #[arg(long, conflicts_with = "description_file")]
        description: Option<String>,
        #[arg(long)]
        description_file: Option<PathBuf>,
        /// Only the deterministic rules: no model, instant (the pre-commit default).
        #[arg(long, conflicts_with = "answers")]
        no_model: bool,
        /// Directory with the model weights; default `$PR_GUARD_MODEL`, else the cache (`$PR_GUARD_CACHE`, `~/.cache/pr-guard`).
        #[arg(long)]
        model: Option<PathBuf>,
        /// ONNX Runtime threads; default all cores.
        #[arg(long)]
        threads: Option<usize>,
        /// Missing file = the default policy.
        #[arg(long, default_value = ".github/pr-guard.yml")]
        policy: PathBuf,
        /// Overrides the policy's mode.
        #[arg(long, value_enum)]
        mode: Option<ModeArg>,
        /// The pull request's labels (repeat the flag).
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Output format; repeat it with one --output each. Default: text to stdout.
        #[arg(long, value_enum)]
        format: Vec<Format>,
        /// Where each --format goes, in the same order (`-` for stdout); one --format alone goes to stdout.
        #[arg(long, value_name = "PATH")]
        output: Vec<PathBuf>,
        /// Where to write the sticky comment's markdown (same as `--format markdown --output <PATH>`).
        #[arg(long, value_name = "PATH")]
        comment: Option<PathBuf>,
        /// The pull request number on the host; default the CI's.
        #[arg(long)]
        pr: Option<String>,
        /// Print only: no comment, labels or statuses on the host even with a token.
        #[arg(long)]
        no_publish: bool,
    },
    /// Re-count a pull request's approvals against the verdict in pr-guard's comment and set the `pr-guard` status again, without
    /// the diff or the model (for review events).
    Reviewers {
        /// The pull request number on the host; default the CI's.
        #[arg(long)]
        pr: Option<String>,
    },
    /// Print the default policy, to start a `.github/pr-guard.yml`.
    Init,
    /// The model package.
    Model {
        #[command(subcommand)]
        cmd: ModelCmd,
    },
}

#[derive(Subcommand)]
enum ModelCmd {
    /// Download the package from Hugging Face ahead of time (Docker images, runners without internet at check time);
    /// `check` downloads it on first use otherwise.
    Pull {
        /// Directory to write it to; default the cache (`$PR_GUARD_MODEL`, `$PR_GUARD_CACHE`, `~/.cache/pr-guard`).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

fn main() {
    // 0 passed, 1 blocked by the policy, 2 usage or runtime error.
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(2);
    }
}

fn run() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Init => print!("{}", serde_yaml::to_string(&Policy::default())?),
        Cmd::Model { cmd: ModelCmd::Pull { dir } } => {
            let dir = dir.or_else(model::default_dir).context("no model directory: pass --dir, set PR_GUARD_MODEL or HOME")?;
            pull::pull(&dir, false)?;
            println!("{}", dir.display());
        }
        Cmd::Check {
            diff: diff_path, staged, base, head, answers, description, description_file, no_model, model: model_dir, threads, policy,
            mode, mut labels, format, output, comment, pr, no_publish,
        } => {
            let targets = targets(format, output, comment)?;
            let mut policy = Policy::load(&policy)?;
            if let Some(m) = mode {
                policy.mode = match m {
                    ModeArg::Shadow => Mode::Shadow,
                    ModeArg::Enforce => Mode::Enforce,
                };
            }
            let env = ci::Env::from_process();
            let ci = ci::detect(&env);
            let http = host::Ureq::new();
            let target = if no_publish { None } else { target(&env, pr.as_deref(), ci.is_some()) };
            // The pull request's own labels, so the skip label works without `--label` in the pipeline.
            if let Some(t) = &target {
                match host::labels(&http, t) {
                    Ok(l) => labels.extend(l),
                    Err(e) => eprintln!("pr-guard: warning: could not read the labels on {}: {e:#}", t.kind.name()),
                }
            }
            let skip = labels.contains(&policy.skip_label);
            let cwd = std::path::Path::new(".");
            let source = match &diff_path {
                Some(_) => None,
                None if staged => Some(git::Source::Staged),
                None => Some(git::range(cwd, base.as_deref(), head.as_deref(), ci.as_ref())?),
            };
            let raw = match (&diff_path, &source) {
                (Some(p), _) if p.as_os_str() == "-" => {
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf).context("reading the diff from stdin")?;
                    String::from_utf8_lossy(&buf).into_owned()
                }
                // As the lab mined it (`errors="replace"`): invalid UTF-8 becomes U+FFFD instead of an error.
                (Some(p), _) => String::from_utf8_lossy(&std::fs::read(p).with_context(|| format!("reading {}", p.display()))?).into_owned(),
                (None, Some(s)) => git::diff(cwd, s)?,
                (None, None) => unreachable!("a source when there is no --diff"),
            };
            let files = diff::parse(&raw);
            // The PR's own description first; commit messages only when there is no PR at hand.
            let description = match description_file {
                Some(p) => Some(std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?),
                None => match description.or_else(|| ci.as_ref().and_then(ci::Ci::description)) {
                    Some(d) => Some(d),
                    None => match &source {
                        Some(s) => git::messages(cwd, s)?,
                        None => None,
                    },
                },
            };
            let mut answers = match answers {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(&p)?).with_context(|| format!("reading {}", p.display()))?,
                None => verdict::Answers::default(),
            };
            if raw.trim().is_empty() {
                eprintln!("pr-guard: no changes to check");
            } else if answers == verdict::Answers::default() && !skip {
                // What the changed files settle first, free and the same without the model; the model only gets the rest.
                answer_by_rule(&mut answers, &facts::files(&raw));
                if !no_model {
                    ask_model(&mut answers, model_dir, threads, description.as_deref(), &raw)?;
                }
            }
            let found = rules::check(&policy.rules, &files)?;
            let v = verdict::decide(&policy, &files, &answers, &labels, found);
            // Workflow commands mean nothing outside GitHub Actions.
            if ci.as_ref().is_some_and(|c| c.provider == ci::Provider::GitHub) {
                print!("{}", report::annotations(&v));
            }
            let color = format::use_color(std::io::IsTerminal::is_terminal(&std::io::stdout()), std::env::var("NO_COLOR").ok().as_deref());
            for (f, path) in targets {
                match path {
                    Some(p) => std::fs::write(&p, format::render(f, &v, false)).with_context(|| format!("writing {}", p.display()))?,
                    None => print!("{}", format::render(f, &v, color)),
                }
            }
            // Failures are warnings: the exit code stays the verdict's.
            if let Some(t) = &target {
                for e in host::publish(&http, &env, t, &policy, &v, &report::comment(&v)) {
                    eprintln!("pr-guard: warning: could not publish to {}: {e}", t.kind.name());
                }
            }
            if v.blocked {
                std::process::exit(1);
            }
        }
        Cmd::Reviewers { pr } => {
            let env = ci::Env::from_process();
            let t = host::target(&env, origin().as_deref(), pr.as_deref()).map_err(|s| anyhow::anyhow!("cannot reach the host: {s}"))?;
            match host::reviewers(&host::Ureq::new(), &env, &t)? {
                Some(d) => eprintln!("pr-guard: {} {}#{}: {d}", t.kind.name(), t.repo, t.pr),
                None => eprintln!("pr-guard: no pr-guard verdict on {} {}#{} yet: nothing to enforce", t.kind.name(), t.repo, t.pr),
            }
        }
    }
    Ok(())
}

/// The `origin` remote's URL, when there is a clone at hand.
fn origin() -> Option<String> {
    git::git(std::path::Path::new("."), &["remote", "get-url", "origin"]).ok().map(|u| u.trim().to_string())
}

/// The pull request on its host, when there is a token and a number; on a laptop nothing is said about it.
fn target(env: &ci::Env, pr: Option<&str>, in_ci: bool) -> Option<host::Target> {
    match host::target(env, origin().as_deref(), pr) {
        Ok(t) => Some(t),
        Err(skip) => {
            if in_ci {
                eprintln!("pr-guard: not publishing: {skip}");
            }
            None
        }
    }
}

/// Pairs each --format with its --output (`None` = stdout): one --output per --format in order, or a single --format to
/// stdout; --comment adds the markdown.
fn targets(formats: Vec<Format>, outputs: Vec<PathBuf>, comment: Option<PathBuf>) -> anyhow::Result<Vec<(Format, Option<PathBuf>)>> {
    let stdout = |p: PathBuf| (p.as_os_str() != "-").then_some(p);
    let mut t = match (formats.len(), outputs.len()) {
        (0, 0) => vec![(Format::Text, None)],
        (0, _) => anyhow::bail!("--output needs a --format before it"),
        (1, 0) => vec![(formats[0], None)],
        (f, o) if f == o => formats.into_iter().zip(outputs.into_iter().map(stdout)).collect(),
        (f, o) => anyhow::bail!("{f} --format but {o} --output: give one --output per --format (`-` for stdout)"),
    };
    if let Some(p) = comment {
        t.push((Format::Markdown, Some(p)));
    }
    Ok(t)
}

/// The change type and the tests question when a rule over the files settles them (`facts`).
fn answer_by_rule(answers: &mut verdict::Answers, files: &[facts::File]) {
    if let Some(t) = facts::change_type(files) {
        eprintln!("pr-guard: prg-tipo rule {t}");
        answers.change_type = Some(t.to_owned());
        answers.change_type_source = Some(Source::Rule);
    }
    if !facts::code_changed(files) {
        eprintln!("pr-guard: prg-testes rule Not applicable: no production code changed");
        answers.tests_applicable = Some(false);
        answers.tests_source = Some(Source::Rule);
    }
}

/// One model answer into `answers`. The tests and type questions come in two shapes: the first packages ask tests as a
/// choice with "Not applicable" and type with 9 classes; from the third round tests is yes/no (option 1 is yes) and the
/// rule answers what is not applicable, and type has the 5 classes that take reading the code.
fn apply(answers: &mut verdict::Answers, q: &model::Question, p: &[f64]) {
    let best = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map_or(0, |(i, _)| i);
    match q.name.as_str() {
        "pr-descricao-noul" => answers.description_matches = Some(p[1]),
        "prg-vul" => answers.vulnerability = Some(p[1]),
        "prg-quebra" => answers.breaking = Some(p[1]),
        "prg-testes" => {
            answers.tests_source = Some(Source::Model);
            match q.kind {
                model::Kind::Noul => {
                    answers.tests_cover = Some(p[1]);
                    answers.tests_applicable = Some(true);
                }
                // Yes, No, Not applicable: not applicable (docs, configuration) is the safe side too.
                model::Kind::Choice => {
                    answers.tests_cover = Some(1.0 - p[1]);
                    answers.tests_applicable = Some(!q.options[best].starts_with("Not applicable"));
                }
            }
        }
        "prg-tipo" => {
            answers.change_type = Some(q.options[best].clone());
            answers.change_type_source = Some(Source::Model);
        }
        "prg-complexidade" => answers.complexity = Some(best as u8),
        // prg-quebra-api: weak on the held-out split (F1 0.48), reported but not used until it is retrained.
        _ => {}
    }
}

fn ask_model(answers: &mut verdict::Answers, dir: Option<PathBuf>, threads: Option<usize>, description: Option<&str>, raw_diff: &str) -> anyhow::Result<()> {
    let description = description.map(model::py_strip).filter(|d| !d.is_empty());
    // A directory the user named is used as it is; the default one is the cache, filled (or brought up to this release's
    // model) from Hugging Face on first use.
    let named = dir.is_some() || std::env::var_os("PR_GUARD_MODEL").is_some();
    let dir = dir.or_else(model::default_dir).context("no model directory: pass --model, set PR_GUARD_MODEL or HOME")?;
    if !named {
        pull::pull(&dir, false).context("getting the model (run with --no-model for the deterministic checks only)")?;
    }
    anyhow::ensure!(
        dir.join("decision_ai.json").exists() || dir.join("pr_guard_model.json").exists(),
        "no model weights in {} (pass --model <dir>, set PR_GUARD_MODEL, or run with --no-model)",
        dir.display()
    );
    let threads = threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let t0 = std::time::Instant::now();
    let mut m = model::Model::load(&dir, threads)?;
    let loaded = t0.elapsed();
    let argmax = |p: &[f64]| p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map_or(0, |(i, _)| i);
    let listed = model::listed_tasks(&dir)?;
    for q in model::questions(listed.as_deref())? {
        // Settled by a rule over the files (`answer_by_rule`).
        let settled = match q.name.as_str() {
            "prg-tipo" => answers.change_type_source == Some(Source::Rule),
            "prg-testes" => answers.tests_source == Some(Source::Rule),
            _ => false,
        };
        if settled || !m.knows(&q.name) {
            continue;
        }
        let Some(p) = m.ask(&q, description, &model::cut_diff(raw_diff, q.diff_chars))? else {
            eprintln!("pr-guard: {} skipped (no description)", q.name);
            continue;
        };
        let best = argmax(&p);
        eprintln!("pr-guard: {:<18} {:>5.3}  {}", q.name, p[best], q.options[best]);
        apply(answers, &q, &p);
    }
    eprintln!("pr-guard: model loaded in {:.1}s, answered in {:.1}s", loaded.as_secs_f64(), (t0.elapsed() - loaded).as_secs_f64());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_pair_with_outputs_in_order() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(targets(vec![], vec![], None).unwrap(), [(Format::Text, None)]);
        assert_eq!(targets(vec![Format::Json], vec![], Some(p("c.md"))).unwrap(), [(Format::Json, None), (Format::Markdown, Some(p("c.md")))]);
        assert_eq!(
            targets(vec![Format::Sarif, Format::Text], vec![p("a.sarif"), p("-")], None).unwrap(),
            [(Format::Sarif, Some(p("a.sarif"))), (Format::Text, None)]
        );
        assert!(targets(vec![Format::Sarif, Format::Junit], vec![p("a")], None).is_err());
        assert!(targets(vec![], vec![p("a")], None).is_err());
    }

    #[test]
    fn tests_and_type_answers_map_from_both_package_shapes() {
        let q = |name: &str, kind: model::Kind, options: &[&str]| model::Question {
            name: name.into(), kind, question: String::new(), options: options.iter().map(|o| o.to_string()).collect(), message: None, diff_chars: model::DIFF_CHARS,
        };
        let old = model::TASKS.iter().find(|t| t.name == "prg-testes").map(model::Question::from).unwrap();
        // The 3-option choice: 1 - P(no) as today, "Not applicable" as the argmax means tests do not apply.
        let mut a = verdict::Answers::default();
        apply(&mut a, &old, &[0.1, 0.2, 0.7]);
        assert_eq!((a.tests_cover, a.tests_applicable, a.tests_source), (Some(0.8), Some(false), Some(Source::Model)));
        apply(&mut a, &old, &[0.6, 0.3, 0.1]);
        assert_eq!(a.tests_applicable, Some(true));
        // The yes/no question: P(yes).
        let noul = q("prg-testes", model::Kind::Noul, &["No, the change needs tests.", "Yes, the tests exercise it."]);
        let mut a = verdict::Answers::default();
        apply(&mut a, &noul, &[0.25, 0.75]);
        assert_eq!((a.tests_cover, a.tests_applicable), (Some(0.75), Some(true)));
        // The type: the argmax option's text, whichever list the package has.
        let five = q("prg-tipo", model::Kind::Choice, &["Bug fix", "New feature", "Refactoring without behavior change", "Performance improvement", "Maintenance or tooling"]);
        apply(&mut a, &five, &[0.1, 0.1, 0.1, 0.1, 0.6]);
        assert_eq!((a.change_type.as_deref(), a.change_type_source), (Some("Maintenance or tooling"), Some(Source::Model)));
    }

    #[test]
    fn rules_settle_the_type_and_the_tests_question_before_the_model() {
        let mut a = verdict::Answers::default();
        answer_by_rule(&mut a, &facts::files("--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-a\n+b\n"));
        assert_eq!((a.change_type.as_deref(), a.change_type_source), (Some(facts::DOCUMENTATION), Some(Source::Rule)));
        assert_eq!((a.tests_cover, a.tests_applicable, a.tests_source), (None, Some(false), Some(Source::Rule)));
        let mut a = verdict::Answers::default();
        answer_by_rule(&mut a, &facts::files("--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\n"));
        assert_eq!(a, verdict::Answers::default());
    }

    #[test]
    fn the_cli_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
