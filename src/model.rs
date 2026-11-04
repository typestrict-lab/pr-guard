//! TS-PRGuard on the CPU: the Hugging Face tokenizer plus ONNX Runtime with 8-bit weights, no API and no key.
//!
//! The text the model reads must be the training text byte for byte; a single token of difference changes the answer
//! and the calibration without raising any error. Every step below is a port of the lab's Python, and
//! `tests/golden.rs` checks it against vectors the Python wrote:
//!
//! 1. `cut_diff`: `corta` in `prguard_montar.py`, production code first, then tests, then the rest, cut at the task's
//!    `diff_chars` (the diff the dataset stores: 5,000 characters, 14,000 for breaking changes).
//! 2. `description_state`: the state `prguard_fumaca.py` builds (description cut at 600 characters, the diff) through
//!    `render` in `typestrict/format.py`, which is `json.dumps(sort_keys=True, ensure_ascii=False, separators=(",", ":"))`.
//! 3. `Encoder::encode`: `Collator.encode` in `typestrict/data/collate.py` without shuffling, as in evaluation:
//!    `[CLS] instructions [SEP] state [SEP] [OPT] criterion ...`, criteria kept, the state cut to what fits in 1,536.
//! 4. `Model::noul`: the 8-bit ONNX from `prguard_onnx.py` (MatMul weights in blocks of 32, embeddings int8), inputs padded to a multiple of 64 (the local attention
//!    works in blocks of 64), and the run's temperature on the two option logits.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow};
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;
use regex::Regex;
use serde::Deserialize;
use tokenizers::Tokenizer;

/// The question as the description smoke run was trained on it (`INS_NOUL`, `CRIT_NOUL` in `d23_code_dados.py`).
pub const DESCRIPTION_INSTRUCTIONS: &str = "Does the code change do what the pull request description says?";
pub const DESCRIPTION_CRITERIA: [&str; 2] =
    ["No, the diff does something different from the description.", "Yes, the diff implements what the description says."];
/// `message[:600]` in `prguard_fumaca.py`.
pub const DESCRIPTION_CHARS: usize = 600;
/// `CORTE` in `prguard_montar.py`.
pub const DIFF_CHARS: usize = 5000;
/// `--corte-quebra` in `prguard_f3.py` (v4, lab HIPOTESES PRG9): the breaking-change question reads the whole PR.
pub const BREAKING_DIFF_CHARS: usize = 14_000;

/// The yes/no and choice questions of the multi-task model, exactly as `scripts/prguard_f3.py` wrote them for the
/// typestrict training (the plan kept them byte for byte). `message` is the state key the PR text goes in, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Noul,
    Choice,
}

#[derive(Debug, Clone, Copy)]
pub struct Task {
    pub name: &'static str,
    pub kind: Kind,
    pub question: &'static str,
    pub options: &'static [&'static str],
    pub message: Option<&'static str>,
    /// Where the diff the task reads is cut (`cut_diff`), as its training data was.
    pub diff_chars: usize,
}

const BREAK: [&str; 2] = ["No, existing users keep working without changes.", "Yes, existing users must change their code or configuration."];

pub const TASKS: [Task; 7] = [
    Task { name: "pr-descricao-noul", kind: Kind::Noul, question: DESCRIPTION_INSTRUCTIONS, options: &DESCRIPTION_CRITERIA, message: Some("description"), diff_chars: DIFF_CHARS },
    Task {
        name: "prg-vul",
        kind: Kind::Noul,
        question: "Does this code change introduce a security vulnerability?",
        options: &["No, the change does not introduce a security vulnerability.", "Yes, the change introduces a security vulnerability."],
        message: None,
        diff_chars: DIFF_CHARS,
    },
    Task { name: "prg-quebra", kind: Kind::Noul, question: "Does this change break compatibility for the users of this code?", options: &BREAK, message: Some("description"), diff_chars: BREAKING_DIFF_CHARS },
    Task { name: "prg-quebra-api", kind: Kind::Noul, question: "Does this change break compatibility for the users of this code?", options: &BREAK, message: Some("commit_message"), diff_chars: DIFF_CHARS },
    Task {
        name: "prg-testes",
        kind: Kind::Choice,
        question: "Do the tests in this change cover the changed code?",
        options: &[
            "Yes, the tests in the change exercise the changed code.",
            "No, the change needs tests and the ones included do not cover it.",
            "Not applicable: the change does not call for tests (docs, configuration, formatting).",
        ],
        message: None,
        diff_chars: DIFF_CHARS,
    },
    Task {
        name: "prg-tipo",
        kind: Kind::Choice,
        question: "What kind of change is this diff?",
        options: &[
            "Bug fix",
            "New feature",
            "Refactoring without behavior change",
            "Performance improvement",
            "Tests only",
            "Build system, dependencies or continuous integration",
            "Maintenance or tooling",
            "Formatting or style only",
            "Documentation only",
        ],
        message: Some("commit_message"),
        diff_chars: DIFF_CHARS,
    },
    Task {
        name: "prg-complexidade",
        kind: Kind::Choice,
        question: "How hard is this change to review?",
        options: &[
            "Trivial: a few lines in one place",
            "Small: a short, focused change",
            "Medium: several files or some new branching logic",
            "Large: many lines or files, or a lot of new branching",
            "Very large: hundreds of lines across many files with much new logic",
        ],
        message: None,
        diff_chars: DIFF_CHARS,
    },
];

/// A question as the model is asked it: a built-in task with the question and options the package lists, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub name: String,
    pub kind: Kind,
    pub question: String,
    pub options: Vec<String>,
    pub message: Option<&'static str>,
    pub diff_chars: usize,
}

impl From<&Task> for Question {
    fn from(t: &Task) -> Self {
        Self {
            name: t.name.into(),
            kind: t.kind,
            question: t.question.into(),
            options: t.options.iter().map(|o| o.to_string()).collect(),
            message: t.message,
            diff_chars: t.diff_chars,
        }
    }
}

/// One entry of `tasks` in typestrict's `decision_ai.json`; `options` is null when each example brings its own.
#[derive(Debug, Clone, Deserialize)]
pub struct ListedTask {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default)]
    pub options: Option<Vec<String>>,
}

/// The package's task list, or `None` when it has none (the first TS-PRGuard, the lab's one-question model).
pub fn listed_tasks(dir: &Path) -> Result<Option<Vec<ListedTask>>> {
    let p = dir.join("decision_ai.json");
    if !p.exists() {
        return Ok(None);
    }
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?)?;
    match m.get("tasks") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(t) => serde_json::from_value(t.clone()).map(Some).context("decision_ai.json: tasks"),
    }
}

/// The questions to ask, in the order of `TASKS`. With a task list, only the listed tasks, each with the package's
/// question and options (the built-in ones where the package gives the options per example); without one, every task.
pub fn questions(listed: Option<&[ListedTask]>) -> Result<Vec<Question>> {
    let Some(listed) = listed else { return Ok(TASKS.iter().map(Question::from).collect()) };
    let mut out = Vec::new();
    for t in &TASKS {
        let Some(l) = listed.iter().find(|l| l.name == t.name) else { continue };
        let mut q = Question::from(t);
        if let Some(options) = &l.options {
            q.kind = match l.kind.as_str() {
                "noul" => Kind::Noul,
                "choice" => Kind::Choice,
                k => anyhow::bail!("decision_ai.json: task {} has kind {k}, which pr-guard does not ask", l.name),
            };
            q.options = options.clone();
        }
        if let Some(question) = &l.question {
            q.question = question.clone();
        }
        out.push(q);
    }
    Ok(out)
}

/// The model package: the lab's `pr_guard_model.json` (one question) or typestrict's `decision_ai.json` (all of them).
#[derive(Debug, Clone, Deserialize)]
pub struct Meta {
    pub onnx: String,
    pub tokenizer: String,
    pub task: String,
    pub max_length: usize,
    pub max_criterion_tokens: usize,
    pub pad_multiple: usize,
    pub cls_id: u32,
    pub sep_id: u32,
    pub pad_id: u32,
    pub opt_id: u32,
    /// (question type: noul, choice, score) x (number of options 2..9).
    pub temperature: Vec<Vec<f64>>,
}

// ---------------------------------------------------------------------------
// Text: the diff the dataset stores and the state the model reads
// ---------------------------------------------------------------------------

const CODE_EXT: &[&str] = &[
    ".js", ".jsx", ".mjs", ".ts", ".tsx", ".go", ".rs", ".py", ".dart", ".kt", ".kts", ".java", ".swift", ".m", ".mm", ".php", ".rb", ".c",
    ".cc", ".cpp", ".h", ".hpp", ".cs", ".scala",
];
// The regexes of `prguard_regras.py`, unchanged.
static TEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)(tests?|spec|specs|__tests__|testdata|[A-Za-z]+Tests)(/|$)|_test\.|_spec\.|\.test\.|\.spec\.|Tests?\.(swift|m)$|(^|/)test_[^/]*$").unwrap()
});
static EXAMPLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(^|/)(test/app|tests?/fixtures?|examples?|samples?|demo|demos|sandbox|playground)(/|$)").unwrap());
static GENERATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\.(pbxproj|xib|storyboard|lock|lockb|min\.js|min\.css|snap|svg|map|pb\.go|g\.dart|freezed\.dart|generated\.\w+)$",
        r"|(^|/)(package-lock\.json|yarn\.lock|pnpm-lock\.yaml|go\.sum|Cargo\.lock|Podfile\.lock|Gemfile\.lock|composer\.lock",
        r"|poetry\.lock|pubspec\.lock|Package\.resolved)$|(^|/)(vendor|node_modules|dist|build|generated)/",
    ))
    .unwrap()
});
static NEW_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\+\+\+ (?:b/)?(\S.*?)\t?$").unwrap());

/// `str.isspace()`: Unicode whitespace plus the four separators U+001C..U+001F.
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()`.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}

/// `Path(f).suffix.lower()`.
fn suffix(f: &str) -> String {
    let name = f.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => name[i..].to_lowercase(),
        _ => String::new(),
    }
}

/// Production code: a code extension, not a test, an example or a generated file.
pub fn is_code(f: &str) -> bool {
    CODE_EXT.contains(&suffix(f).as_str()) && !TEST.is_match(f) && !EXAMPLE.is_match(f) && !GENERATED.is_match(f)
}

fn is_test(f: &str) -> bool {
    TEST.is_match(f) && !EXAMPLE.is_match(f)
}

/// The test regex alone (fixtures under a test directory count too).
pub fn in_tests(f: &str) -> bool {
    TEST.is_match(f)
}

/// `blocos`: (file, text) per file of a unified diff. A part starts at each line that begins with `diff --git `,
/// `--- a/` or `--- /dev/null`; a lone `diff --git` header (binary, pure rename) goes with the next part.
fn blocks(diff: &str) -> Vec<(String, String)> {
    let mut cuts = vec![0];
    let mut at = 0;
    for line in diff.split_inclusive('\n') {
        let rest = &diff[at..];
        if at > 0 && (rest.starts_with("diff --git ") || rest.starts_with("--- a/") || rest.starts_with("--- /dev/null")) {
            cuts.push(at);
        }
        at += line.len();
    }
    cuts.push(diff.len());
    let mut out = Vec::new();
    let mut pending = String::new();
    for w in cuts.windows(2) {
        let part = &diff[w[0]..w[1]];
        if py_strip(part).is_empty() {
            continue;
        }
        if part.starts_with("diff --git") && !part.contains("\n--- ") && !part.contains("\n+++ ") {
            pending.push_str(part);
            continue;
        }
        let file = NEW_FILE.captures(part).map(|c| c[1].to_string()).unwrap_or_default();
        out.push((file, std::mem::take(&mut pending) + part));
    }
    out
}

/// `corta`: production code first, then tests, then the rest (changelog, version), so a size cut does not eat the
/// change; then cut at `chars` characters.
pub fn cut_diff(diff: &str, chars: usize) -> String {
    let mut b = blocks(diff);
    b.sort_by_key(|(f, _)| if is_code(f) { 0 } else if is_test(f) { 1 } else { 2 });
    let d: String = b.into_iter().map(|(_, t)| t).collect();
    match d.char_indices().nth(chars) {
        Some((i, _)) => format!("{}\n... (diff cut)", &d[..i]),
        None => d,
    }
}

/// A JSON string as Python's `json.dumps(ensure_ascii=False)` writes it.
fn py_json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A state as `render` writes it: sorted keys, each message cut at `DESCRIPTION_CHARS`.
pub fn state(message: Option<(&str, &str)>, diff: &str) -> String {
    let mut s = String::with_capacity(diff.len() + 700);
    s.push('{');
    if let Some((key, text)) = message {
        let text = match text.char_indices().nth(DESCRIPTION_CHARS) {
            Some((i, _)) => &text[..i],
            None => text,
        };
        s.push('"');
        s.push_str(key);
        s.push_str("\":");
        py_json_str(text, &mut s);
        s.push(',');
    }
    s.push_str("\"diff\":");
    py_json_str(diff, &mut s);
    s.push('}');
    s
}

/// The state of the description question, rendered: `{"description":...,"diff":...}` (sorted keys, so the description
/// comes first and the 1,536/2,048-token cut eats the end of the diff, never the description).
/// `diff` is the stored diff (`cut_diff` of the raw one); the description is cut at 600 characters here.
pub fn description_state(description: &str, diff: &str) -> String {
    state(Some(("description", description)), diff)
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Encoded {
    pub ids: Vec<u32>,
    pub opt_positions: Vec<usize>,
}

/// Tokenizer and sequence layout, apart from the session so the ids can be tested without the ONNX.
pub struct Encoder {
    tok: Tokenizer,
    pub meta: Meta,
}

impl Encoder {
    pub fn load(dir: &Path) -> Result<Self> {
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).with_context(|| format!("reading {}", dir.join(f).display()));
        if !dir.join("decision_ai.json").exists() {
            let meta: Meta = serde_json::from_str(&read("pr_guard_model.json")?)?;
            let tok = Tokenizer::from_file(dir.join(&meta.tokenizer)).map_err(|e| anyhow!("tokenizer: {e}"))?;
            return Ok(Self { tok, meta });
        }
        // typestrict's package (`package` in trainer/run.py): token names instead of ids, the training cuts under
        // `truncation`, calibration null when the run kept the raw logits.
        let m: serde_json::Value = serde_json::from_str(&read("decision_ai.json")?)?;
        anyhow::ensure!(m["format"] == "decision-ai/option-reader@1", "decision_ai.json: unknown format {}", m["format"]);
        let s = |v: &serde_json::Value| v.as_str().map(str::to_owned).context("decision_ai.json: missing field");
        let tokenizer = s(&m["files"]["tokenizer"])?;
        let tok = Tokenizer::from_file(dir.join(&tokenizer)).map_err(|e| anyhow!("tokenizer: {e}"))?;
        let id = |k: &str| -> Result<u32> {
            let name = s(&m["tokens"][k])?;
            tok.token_to_id(&name).with_context(|| format!("tokenizer has no {name}"))
        };
        let n = |v: &serde_json::Value| v.as_u64().map(|x| x as usize).context("decision_ai.json: missing number");
        let t = m["calibration"]["temperature"].as_f64().unwrap_or(1.0);
        let meta = Meta {
            onnx: s(&m["files"]["model"])?,
            tokenizer,
            task: "multitask".into(),
            max_length: n(&m["truncation"]["max_length"])?,
            max_criterion_tokens: n(&m["truncation"]["max_criterion_tokens"])?,
            pad_multiple: n(&m["pad_multiple"])?,
            cls_id: id("cls")?,
            sep_id: id("sep")?,
            pad_id: id("pad")?,
            opt_id: id("option")?,
            temperature: vec![vec![t; 8]; 3],
        };
        Ok(Self { tok, meta })
    }

    fn ids(&self, text: &str, limit: Option<usize>) -> Result<Vec<u32>> {
        if text.is_empty() {
            return Ok(vec![]);
        }
        let mut v = self.tok.encode(text, false).map_err(|e| anyhow!("tokenize: {e}"))?.get_ids().to_vec();
        if let Some(l) = limit {
            v.truncate(l);
        }
        Ok(v)
    }

    /// `[CLS] instructions [SEP] state [SEP] ([OPT] criterion)...`: the criteria are reserved first (each capped at
    /// `max_criterion_tokens`) and the state keeps what is left of `max_length`.
    pub fn encode(&self, instructions: &str, criteria: &[&str], state: &str) -> Result<Encoded> {
        let m = &self.meta;
        let crit = criteria.iter().map(|c| self.ids(py_strip(c), Some(m.max_criterion_tokens))).collect::<Result<Vec<_>>>()?;
        let instr = self.ids(py_strip(instructions), None)?;
        let reserved = 3 + instr.len() + crit.iter().map(|c| 1 + c.len()).sum::<usize>();
        anyhow::ensure!(reserved < m.max_length, "the criteria need {reserved} tokens, more than max_length {}", m.max_length);
        let state = self.ids(state, Some(m.max_length - reserved))?;
        let mut ids = Vec::with_capacity(m.max_length);
        ids.push(m.cls_id);
        ids.extend(&instr);
        ids.push(m.sep_id);
        ids.extend(&state);
        ids.push(m.sep_id);
        let mut opt_positions = Vec::with_capacity(crit.len());
        for c in &crit {
            opt_positions.push(ids.len());
            ids.push(m.opt_id);
            ids.extend(c);
        }
        Ok(Encoded { ids, opt_positions })
    }

    /// The description question for a description and a diff as the dataset stores it (already cut).
    pub fn description(&self, description: &str, diff: &str) -> Result<Encoded> {
        self.encode(DESCRIPTION_INSTRUCTIONS, &DESCRIPTION_CRITERIA, &description_state(description, diff))
    }
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

pub struct Model {
    pub enc: Encoder,
    session: Session,
}

impl Model {
    /// `dir` holds `pr_guard_model.json`, the tokenizer and the 8-bit ONNX.
    pub fn load(dir: &Path, threads: usize) -> Result<Self> {
        let enc = Encoder::load(dir)?;
        anyhow::ensure!(
            ["pr-descricao-noul", "multitask"].contains(&enc.meta.task.as_str()),
            "{}: model for {}, not TS-PRGuard",
            dir.display(),
            enc.meta.task
        );
        // The builder's errors carry the builder, which is not Sync: they become text.
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow!("{e}"))?
            .with_intra_threads(threads)
            .map_err(|e| anyhow!("{e}"))?
            .with_inter_threads(1)
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(dir.join(&enc.meta.onnx))
            .with_context(|| format!("loading {}", dir.join(&enc.meta.onnx).display()))?;
        Ok(Self { enc, session })
    }

    /// Whether this model was trained on `task` (the lab's smoke model knows only the description question).
    pub fn knows(&self, task: &str) -> bool {
        self.enc.meta.task == "multitask" || self.enc.meta.task == task
    }

    /// The probability of each option of `task`, or `None` when the task needs the PR text and there is none.
    /// `cut` is the diff as the dataset stores it (`cut_diff` at the question's `diff_chars`).
    pub fn ask(&mut self, q: &Question, message: Option<&str>, cut: &str) -> Result<Option<Vec<f64>>> {
        let state = match (q.message, message) {
            (Some(key), Some(m)) => state(Some((key, m)), cut),
            (Some(_), None) => return Ok(None),
            (None, _) => state(None, cut),
        };
        let options: Vec<&str> = q.options.iter().map(String::as_str).collect();
        let e = self.enc.encode(&q.question, &options, &state)?;
        self.probs(&e, q.kind).map(Some)
    }

    /// Softmax over the options' logits, each over the run's temperature for (question type, number of options).
    pub fn probs(&mut self, e: &Encoded, kind: Kind) -> Result<Vec<f64>> {
        let m = &self.enc.meta;
        let t = e.ids.len().div_ceil(m.pad_multiple) * m.pad_multiple;
        let mut ids = vec![i64::from(m.pad_id); t];
        let mut pad_mask = vec![false; t];
        for (j, &id) in e.ids.iter().enumerate() {
            ids[j] = i64::from(id);
            pad_mask[j] = true;
        }
        let n = e.opt_positions.len();
        anyhow::ensure!((2..=9).contains(&n), "a question has 2 to 9 options, got {n}");
        let q = match kind {
            Kind::Noul => 0usize,
            Kind::Choice => 1,
        };
        let temp = m.temperature[q][n - 2];
        let pos: Vec<i64> = e.opt_positions.iter().map(|&p| p as i64).collect();
        let out = self.session.run(ort::inputs![
            "ids" => Tensor::from_array(([1, t], ids))?,
            "opt_positions" => Tensor::from_array(([1, n], pos))?,
            "qtype" => Tensor::from_array(([1], vec![q as i64]))?,
            "pad_mask" => Tensor::from_array(([1, t], pad_mask))?,
            "opt_mask" => Tensor::from_array(([1, n], vec![true; n]))?,
        ])?;
        let (_, logits) = out["logits"].try_extract_tensor::<f32>()?;
        let z: Vec<f64> = logits[..n].iter().map(|&l| f64::from(l) / temp).collect();
        let max = z.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let e: Vec<f64> = z.iter().map(|v| (v - max).exp()).collect();
        let sum: f64 = e.iter().sum();
        Ok(e.into_iter().map(|v| v / sum).collect())
    }

    /// Calibrated probability of "yes" for an encoded yes/no question: the logits of the two options over the run's
    /// temperature for (noul, 2 options), then softmax.
    pub fn noul(&mut self, e: &Encoded) -> Result<f64> {
        anyhow::ensure!(e.opt_positions.len() == 2, "a yes/no question has two options, got {}", e.opt_positions.len());
        Ok(self.probs(e, Kind::Noul)?[1])
    }

    /// Does the change do what the description says? `raw_diff` is the diff as git prints it.
    pub fn description_matches(&mut self, description: &str, raw_diff: &str) -> Result<f64> {
        let e = self.enc.description(description, &cut_diff(raw_diff, DIFF_CHARS))?;
        self.noul(&e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_is_python_json_with_sorted_keys_and_raw_unicode() {
        assert_eq!(
            description_state("Fix \"x\"\t\u{1b}é\u{2028}", "a\\b\n</>&"),
            "{\"description\":\"Fix \\\"x\\\"\\t\\u001bé\u{2028}\",\"diff\":\"a\\\\b\\n</>&\"}"
        );
        let long = "é".repeat(700);
        assert_eq!(description_state(&long, ""), format!("{{\"description\":\"{}\",\"diff\":\"\"}}", "é".repeat(600)));
    }

    #[test]
    fn code_comes_first_then_tests_then_the_rest_and_a_lone_header_joins_the_next_file() {
        let d = "diff --git a/CHANGELOG.md b/CHANGELOG.md\n--- a/CHANGELOG.md\n+++ b/CHANGELOG.md\n@@ -1 +1 @@\n+x\n\
                 diff --git a/logo.png b/logo.png\nBinary files differ\n\
                 diff --git a/src/a_test.go b/src/a_test.go\n--- a/src/a_test.go\n+++ b/src/a_test.go\n@@ -1 +1 @@\n+t\n\
                 diff --git a/src/a.go b/src/a.go\n--- a/src/a.go\n+++ b/src/a.go\n@@ -1 +1 @@\n+c\n";
        let c = cut_diff(d, DIFF_CHARS);
        let order: Vec<usize> = ["src/a.go", "src/a_test.go", "CHANGELOG.md"].iter().map(|f| c.find(&format!("+++ b/{f}")).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{c}");
        assert!(c.find("logo.png").unwrap() < c.find("+++ b/src/a_test.go").unwrap());
        assert_eq!(cut_diff(&"x".repeat(5001), DIFF_CHARS), format!("{}\n... (diff cut)", "x".repeat(5000)));
        assert_eq!(cut_diff(&"x".repeat(5001), BREAKING_DIFF_CHARS), "x".repeat(5001));
    }

    #[test]
    fn the_package_task_list_picks_and_shapes_the_questions() {
        let dir = std::env::temp_dir().join(format!("pr-guard-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // No decision_ai.json, or one without a task list: every built-in task as written.
        assert!(listed_tasks(&dir).unwrap().is_none());
        std::fs::write(dir.join("decision_ai.json"), r#"{"format":"decision-ai/option-reader@1"}"#).unwrap();
        assert!(listed_tasks(&dir).unwrap().is_none());
        assert_eq!(questions(None).unwrap().len(), TASKS.len());
        std::fs::write(
            dir.join("decision_ai.json"),
            r#"{"tasks":[
                {"name":"prg-tipo","kind":"choice","question":"What kind of change is this diff?","options":["Bug fix","New feature"]},
                {"name":"prg-testes","kind":"noul","question":"Do the tests in this change cover the changed code?",
                 "options":["No, the change needs tests and the ones included do not cover it.","Yes, the tests in the change exercise the changed code."]},
                {"name":"prg-vul","kind":"noul","question":"Is this change unsafe?","options":null},
                {"name":"someone-else","kind":"score","question":"?","options":["a","b"]}
            ]}"#,
        )
        .unwrap();
        let listed = listed_tasks(&dir).unwrap().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let q = questions(Some(&listed)).unwrap();
        // In the order of TASKS; unlisted ones (prg-quebra-api among them) and unknown names left out.
        assert_eq!(q.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(), ["prg-vul", "prg-testes", "prg-tipo"]);
        // Null options: the built-in kind and options, the package's question.
        assert_eq!((q[0].kind, q[0].question.as_str(), q[0].options.len()), (Kind::Noul, "Is this change unsafe?", 2));
        assert_eq!(q[1].kind, Kind::Noul);
        assert!(q[1].options[1].starts_with("Yes"));
        assert_eq!((q[2].kind, q[2].options.clone()), (Kind::Choice, vec!["Bug fix".to_string(), "New feature".into()]));
        assert_eq!(q[2].message, Some("commit_message"));
        let bad = [ListedTask { name: "prg-vul".into(), kind: "score".into(), question: None, options: Some(vec!["a".into(), "b".into()]) }];
        assert!(questions(Some(&bad)).is_err());
    }
}
