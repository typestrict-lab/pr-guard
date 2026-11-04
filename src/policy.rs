//! The repository's policy (`.github/pr-guard.yml`): how much each answer weighs, the minimums, the reviewer bands and
//! the deterministic rules. Every field has a default, so an empty file is a valid policy.
//!
//! The file speaks as the comment does (percentages of the risky answer, size bands by name, levels as block, warn,
//! note or off) and is read into `Policy`, which decides on scores where 1 is the safe side. `DEFAULT_FILE` is both what
//! `pr-guard init` prints and, read, `Policy::default()`.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warning,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Reports on every pull request but never blocks, labels for merge or fails the check: for calibrating a new policy.
    Shadow,
    #[default]
    Enforce,
}

/// The questions TS-PRGuard answers. `ChangeType` only labels; the others carry a score where 1 is the safe side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Question {
    DescriptionMatches,
    Complexity,
    Vulnerability,
    TestsCover,
    Breaking,
    ResolvesIssue,
    Focused,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    #[serde(default = "one")]
    pub weight: f64,
    /// Below this the answer is a warning (at least one reviewer).
    #[serde(default)]
    pub warn_below: Option<f64>,
    /// Below this the answer is a danger (two reviewers; blocks when `block_on: danger`).
    #[serde(default)]
    pub min: Option<f64>,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reviewers {
    /// Combined score at or above this: no reviewer needed (auto-merge when allowed).
    pub none_at: f64,
    /// At or above this: one reviewer; below: two.
    pub one_at: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    pub paths: Vec<String>,
    pub level: Level,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LevelRule {
    pub level: Level,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeRule {
    pub max_lines: u32,
    pub level: Level,
}

/// Each rule can be turned off with `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Rules {
    pub sensitive_paths: Option<PathRule>,
    pub secrets: Option<LevelRule>,
    pub dependencies: Option<LevelRule>,
    pub size: Option<SizeRule>,
    pub destructive_sql: Option<LevelRule>,
}

impl Default for Rules {
    fn default() -> Self {
        let paths = [".github/workflows/**", "**/auth/**", "**/*auth*.*", "**/migrations/**", "**/Dockerfile", "**/*.tf", "**/.env*"];
        Rules {
            sensitive_paths: Some(PathRule { paths: paths.map(String::from).to_vec(), level: Level::Warning }),
            secrets: Some(LevelRule { level: Level::Danger }),
            dependencies: Some(LevelRule { level: Level::Info }),
            size: Some(SizeRule { max_lines: 800, level: Level::Warning }),
            destructive_sql: Some(LevelRule { level: Level::Danger }),
        }
    }
}

/// What `check` writes on the pull request when it has a host token (`host`); each part can be turned off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Publish {
    /// The sticky comment, which also keeps the verdict for `pr-guard reviewers`.
    pub comment: bool,
    /// `pr-guard:reviewers-N`, `pr-guard:blocked` and the others in the verdict (no labels on Bitbucket).
    pub labels: bool,
    /// The `pr-guard` commit status: failed when the policy blocks, waiting for the approvals (when `reviewers`).
    pub status: bool,
    /// Required approvals: the `pr-guard` status waits for them (GitHub, Bitbucket), or the approval rule (GitLab).
    pub reviewers: bool,
}

impl Default for Publish {
    fn default() -> Self {
        Publish { comment: true, labels: true, status: true, reviewers: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Policy {
    pub mode: Mode,
    /// The model package (`crate::pull::parse`).
    pub model: String,
    pub skip_label: String,
    pub questions: BTreeMap<Question, Gate>,
    pub reviewers: Reviewers,
    /// Lets a pull request that needs no reviewer be labeled for auto-merge.
    pub auto_merge: bool,
    /// Findings at this level or above fail the check; `null` never fails it.
    pub block_on: Option<Level>,
    pub rules: Rules,
    /// Languages the model was evaluated on; code in any other language is "not evaluated" and never auto-merges.
    pub languages: Vec<String>,
    pub publish: Publish,
}

impl Default for Policy {
    fn default() -> Self {
        let g = |weight, warn_below, min| Gate { weight, warn_below, min };
        Policy {
            // Shadow until a repository has seen what the guard says on its own pull requests: report, never fail.
            mode: Mode::Shadow,
            model: crate::pull::DEFAULT.into(),
            skip_label: "pr-guard:skip".into(),
            questions: BTreeMap::from([
                (Question::DescriptionMatches, g(2.0, Some(0.6), Some(0.3))),
                (Question::Complexity, g(1.0, Some(0.25), None)),
                // A warning from P >= 0.7 (95% precise on TS-PRGuard v1's held-out fixes), never a danger: those fixes are
                // more than half of the test set, a real repository's pull requests almost never are, and the scenarios in
                // examples/ flagged 6 of 27 clean changes. Set `min` to block on it.
                (Question::Vulnerability, g(3.0, Some(0.3), None)),
                (Question::TestsCover, g(2.0, Some(0.5), None)),
                (Question::Breaking, g(2.0, Some(0.5), None)),
                (Question::ResolvesIssue, g(1.0, Some(0.5), None)),
                (Question::Focused, g(1.0, Some(0.5), None)),
            ]),
            reviewers: Reviewers { none_at: 0.85, one_at: 0.6 },
            auto_merge: false,
            block_on: Some(Level::Danger),
            rules: Rules::default(),
            languages: crate::lang::SUPPORTED.iter().map(|s| s.to_string()).collect(),
            publish: Publish::default(),
        }
    }
}

impl Policy {
    pub fn from_yaml(s: &str) -> anyhow::Result<Policy> {
        let f: File = if s.trim().is_empty() { File::default() } else { serde_yaml::from_str(s)? };
        f.into_policy()
    }

    /// A missing file is the default policy. So is a path that cannot exist because a parent is a file
    /// (`/dev/null/none`): both mean "no policy here", not a broken one.
    pub fn load(path: &Path) -> anyhow::Result<Policy> {
        match std::fs::read_to_string(path) {
            Ok(s) => Policy::from_yaml(&s).with_context(|| format!("reading {}", path.display())),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(Policy::default()),
            Err(e) => Err(e).context(format!("reading {}", path.display())),
        }
    }
}

/// The policy `pr-guard init` prints; reading it gives `Policy::default()` (a test holds them together).
pub const DEFAULT_FILE: &str = include_str!("default-policy.yml");

// --- The file as people write it ---

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    mode: Option<Mode>,
    model: Option<String>,
    reviewers: Option<FileReviewers>,
    questions: Option<BTreeMap<FileQuestion, FileGate>>,
    rules: Option<FileRules>,
    skip_label: Option<String>,
    auto_merge: Option<bool>,
    languages: Option<Vec<String>>,
    publish: Option<Publish>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileReviewers {
    none: Pct,
    one: Pct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FileQuestion {
    Description,
    Vulnerability,
    Breaking,
    Tests,
    Size,
    Issue,
    Focus,
}

impl FileQuestion {
    fn question(self) -> Question {
        match self {
            FileQuestion::Description => Question::DescriptionMatches,
            FileQuestion::Vulnerability => Question::Vulnerability,
            FileQuestion::Breaking => Question::Breaking,
            FileQuestion::Tests => Question::TestsCover,
            FileQuestion::Size => Question::Complexity,
            FileQuestion::Issue => Question::ResolvesIssue,
            FileQuestion::Focus => Question::Focused,
        }
    }
}

/// A question's entry: what it is missing keeps its default; `off` drops a threshold.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileGate {
    weight: Option<f64>,
    warn: Option<Threshold>,
    block: Option<Threshold>,
}

/// `70%` (or 0.7) of the risky answer, a size band by name, or `off`.
#[derive(Debug, Clone, Copy)]
enum Threshold {
    Off,
    Risk(f64),
    Band(u8),
}

const BANDS: [&str; 5] = ["trivial", "small", "medium", "large", "very large"];

impl<'de> Deserialize<'de> for Threshold {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_yaml::Value::deserialize(d)?;
        if let Some(s) = v.as_str() {
            let s = s.trim().to_ascii_lowercase();
            if s == "off" {
                return Ok(Threshold::Off);
            }
            if let Some(b) = BANDS.iter().position(|b| *b == s) {
                return Ok(Threshold::Band(b as u8));
            }
        }
        Pct::from_value(&v).map(|p| Threshold::Risk(p.0)).map_err(serde::de::Error::custom)
    }
}

/// A share written `85%`, `0.85` or `85`.
#[derive(Debug, Clone, Copy)]
struct Pct(f64);

impl Pct {
    fn from_value(v: &serde_yaml::Value) -> Result<Pct, String> {
        let n = match v {
            serde_yaml::Value::Number(n) => n.as_f64().map(|n| if n > 1.0 { n / 100.0 } else { n }),
            serde_yaml::Value::String(s) => s.trim().strip_suffix('%').and_then(|n| n.trim().parse::<f64>().ok()).map(|n| n / 100.0),
            _ => None,
        };
        n.filter(|n| (0.0..=1.0).contains(n)).map(Pct).ok_or_else(|| format!("expected a percentage like 70%, got {v:?}"))
    }
}

impl<'de> Deserialize<'de> for Pct {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Pct::from_value(&serde_yaml::Value::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FileLevel {
    Block,
    Warn,
    Note,
    Off,
}

impl FileLevel {
    fn level(self) -> Option<Level> {
        match self {
            FileLevel::Block => Some(Level::Danger),
            FileLevel::Warn => Some(Level::Warning),
            FileLevel::Note => Some(Level::Info),
            FileLevel::Off => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRules {
    secrets: Option<FileLevel>,
    destructive_sql: Option<FileLevel>,
    dependencies: Option<FileLevel>,
    big_change: Option<OffOr<FileBigChange>>,
    sensitive_paths: Option<OffOr<FilePaths>>,
}

/// `off`, or the rule's settings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OffOr<T> {
    Level(FileLevel),
    Set(T),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBigChange {
    lines: Option<u32>,
    level: Option<FileLevel>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePaths {
    paths: Option<Vec<String>>,
    level: Option<FileLevel>,
}

impl File {
    fn into_policy(self) -> anyhow::Result<Policy> {
        let mut p = Policy::default();
        if let Some(m) = self.mode {
            p.mode = m;
        }
        if let Some(m) = self.model {
            crate::pull::parse(&m)?;
            p.model = m;
        }
        if let Some(r) = self.reviewers {
            anyhow::ensure!(r.one.0 <= r.none.0, "reviewers.one must not exceed reviewers.none");
            p.reviewers = Reviewers { none_at: r.none.0, one_at: r.one.0 };
        }
        for (q, g) in self.questions.unwrap_or_default() {
            let size = q == FileQuestion::Size;
            let gate = p.questions.entry(q.question()).or_insert(Gate { weight: 1.0, warn_below: None, min: None });
            // The file speaks of the risky answer (or the size band); the policy of the score on the safe side, where a
            // band b is 1 - b/4 and the answer acts below the score one band under it.
            let safe = |t: Threshold| -> anyhow::Result<Option<f64>> {
                Ok(match (t, size) {
                    (Threshold::Off, _) => None,
                    (Threshold::Band(b), true) => Some((5.0 - f64::from(b)) / 4.0),
                    // Rounded: 1 - 0.7 is 0.30000000000000004, and a threshold reads as written.
                    (Threshold::Risk(r), false) => Some(((1.0 - r) * 1e6).round() / 1e6),
                    (Threshold::Band(_), false) => anyhow::bail!("size bands are for `size`; the other questions take a percentage"),
                    (Threshold::Risk(_), true) => anyhow::bail!("size takes a band: trivial, small, medium, large or very large"),
                })
            };
            if let Some(w) = g.weight {
                anyhow::ensure!(w >= 0.0, "a weight cannot be negative");
                gate.weight = w;
            }
            if let Some(t) = g.warn {
                gate.warn_below = safe(t)?;
            }
            if let Some(t) = g.block {
                gate.min = safe(t)?;
            }
        }
        if let Some(r) = self.rules {
            let one = |l: FileLevel| l.level().map(|level| LevelRule { level });
            if let Some(l) = r.secrets {
                p.rules.secrets = one(l);
            }
            if let Some(l) = r.destructive_sql {
                p.rules.destructive_sql = one(l);
            }
            if let Some(l) = r.dependencies {
                p.rules.dependencies = one(l);
            }
            match r.big_change {
                Some(OffOr::Level(l)) => p.rules.size = l.level().map(|level| SizeRule { max_lines: 800, level }),
                Some(OffOr::Set(b)) => {
                    let d = p.rules.size.unwrap_or(SizeRule { max_lines: 800, level: Level::Warning });
                    p.rules.size = b.level.map_or(Some(Level::Warning), FileLevel::level).map(|level| SizeRule { max_lines: b.lines.unwrap_or(d.max_lines), level });
                }
                None => {}
            }
            match r.sensitive_paths {
                Some(OffOr::Level(l)) => p.rules.sensitive_paths = l.level().zip(p.rules.sensitive_paths.take()).map(|(level, r)| PathRule { level, ..r }),
                Some(OffOr::Set(f)) => {
                    let d = p.rules.sensitive_paths.take().unwrap_or(PathRule { paths: vec![], level: Level::Warning });
                    p.rules.sensitive_paths = f.level.map_or(Some(Level::Warning), FileLevel::level).map(|level| PathRule { paths: f.paths.unwrap_or(d.paths), level });
                }
                None => {}
            }
        }
        if let Some(l) = self.skip_label {
            p.skip_label = l;
        }
        if let Some(a) = self.auto_merge {
            p.auto_merge = a;
        }
        if let Some(l) = self.languages {
            p.languages = l;
        }
        if let Some(x) = self.publish {
            p.publish = x;
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_the_default_and_partial_files_keep_the_other_defaults() {
        assert_eq!(Policy::from_yaml("").unwrap(), Policy::default());
        let p = Policy::from_yaml("mode: enforce\nrules:\n  big_change: off\n").unwrap();
        assert_eq!(p.mode, Mode::Enforce);
        assert!(p.rules.size.is_none() && p.rules.secrets.is_some());
        assert_eq!(p.reviewers, Policy::default().reviewers);
        let p = Policy::from_yaml("publish:\n  labels: false\n").unwrap();
        assert!(!p.publish.labels && p.publish.comment && p.publish.reviewers);
    }

    #[test]
    fn typos_and_inverted_bands_are_errors() {
        assert!(Policy::from_yaml("reviewrs: {}").is_err());
        assert!(Policy::from_yaml("reviewers: {none: 50%, one: 80%}").is_err());
        assert!(Policy::from_yaml("questions: {size: {warn: 50%}}").is_err());
        assert!(Policy::from_yaml("questions: {tests: {warn: large}}").is_err());
        assert!(Policy::from_yaml("questions: {tests: {warn: 150%}}").is_err());
        assert!(Policy::from_yaml("model: something").is_err());
    }

    #[test]
    fn the_printed_default_is_the_default() {
        assert_eq!(Policy::from_yaml(DEFAULT_FILE).unwrap(), Policy::default());
    }

    #[test]
    fn the_file_speaks_of_the_risky_answer_in_percent_and_sizes_by_name() {
        let p = Policy::from_yaml(
            "model: hf:acme/guard@v2\nreviewers: {none: 90%, one: 0.7}\nquestions:\n  vulnerability: {block: 80%}\n  size: {warn: large, block: very large}\n  tests: {warn: off}\nrules:\n  secrets: warn\n  dependencies: off\n  sensitive_paths: {paths: [infra/**]}\n",
        )
        .unwrap();
        assert_eq!(p.model, "hf:acme/guard@v2");
        assert_eq!(p.reviewers, Reviewers { none_at: 0.9, one_at: 0.7 });
        let v = p.questions[&Question::Vulnerability];
        assert_eq!((v.weight, v.warn_below, v.min.map(|m| (m * 100.0).round())), (3.0, Some(0.3), Some(20.0)));
        // large is band 3 (score 0.25): warn below the score of band 2, so at large and above.
        assert_eq!((p.questions[&Question::Complexity].warn_below, p.questions[&Question::Complexity].min), (Some(0.5), Some(0.25)));
        assert_eq!(p.questions[&Question::TestsCover].warn_below, None);
        assert_eq!(p.rules.secrets, Some(LevelRule { level: Level::Warning }));
        assert!(p.rules.dependencies.is_none());
        assert_eq!(p.rules.sensitive_paths, Some(PathRule { paths: vec!["infra/**".into()], level: Level::Warning }));
    }

    #[test]
    fn a_path_under_a_file_is_missing_like_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("pr-guard-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("policy.yml");
        std::fs::write(&file, "mode: enforce\n").unwrap();
        assert_eq!(Policy::load(&file).unwrap().mode, Mode::Enforce);
        assert_eq!(Policy::load(&dir.join("absent.yml")).unwrap(), Policy::default());
        assert_eq!(Policy::load(&file.join("none")).unwrap(), Policy::default());
        #[cfg(unix)]
        assert_eq!(Policy::load(Path::new("/dev/null/none")).unwrap(), Policy::default());
        std::fs::write(&file, "reviewrs: {}\n").unwrap();
        assert!(Policy::load(&file).is_err());
        // A directory is not a policy: that is a mistake to report, not a default.
        assert!(Policy::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
