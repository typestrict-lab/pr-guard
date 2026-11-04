//! Which model package to use and where it comes from: the `model` of the policy, `--model` or `$PR_GUARD_MODEL`.
//!
//! - `pr-guard:1` (the default), a release of TS-PRGuard this binary knows: a pinned commit of its Hugging Face repo
//!   and the SHA-256 of its manifest, so every byte is the one the release was tested with. `pr-guard` alone is the
//!   newest of them.
//! - `pr-guard:latest`, the repo's `main` as it is now (new releases before the CLI pins them).
//! - `hf:<owner>/<repo>[@<revision>]`, any Hugging Face repo with the same three files (a model trained on your own
//!   pull requests); a private one with `$HF_TOKEN`.
//! - `https://…/`, a base URL serving the files (a CDN, an internal mirror); the URL should carry the version, since
//!   it is downloaded once.
//! - a directory, used as it is.
//!
//! Every file is checked against the SHA-256 the manifest lists; each is written as `.part` and renamed once verified,
//! the manifest last, so an interrupted download never leaves a package that looks complete.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub const DEFAULT: &str = "pr-guard:1";
const REPO: &str = "typestrict/TS-PRGuard";
/// The releases this binary pins: version, commit of the repo, SHA-256 of its `decision_ai.json`. The scoreboard in
/// examples/ ran against the last one.
const RELEASES: [(&str, &str, &str); 1] = [("1", "2179dee21c568da8c75304c0fd6dd3c97a079b3c", "c33899c0ba2de2d7929abfb97e7dd72b2b16e9f19e8895c9e4c0a04055cc03f8")];
const FILES: [&str; 2] = ["tokenizer.json", "onnx/model_8bit.onnx"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Release { version: &'static str, revision: &'static str, manifest: &'static str },
    Hub { repo: String, revision: String },
    Url(String),
    Dir(PathBuf),
}

pub fn parse(s: &str) -> Result<Source> {
    let s = s.trim();
    if let Some(v) = s.strip_prefix("pr-guard").filter(|r| r.is_empty() || r.starts_with(':')) {
        return match v.trim_start_matches(':') {
            "" => Ok(release(RELEASES[RELEASES.len() - 1])),
            "latest" => Ok(Source::Hub { repo: REPO.into(), revision: "main".into() }),
            v => RELEASES.iter().find(|r| r.0 == v.trim_start_matches('v')).map(|r| release(*r)).with_context(|| {
                let known: Vec<_> = RELEASES.iter().map(|r| format!("pr-guard:{}", r.0)).collect();
                format!("unknown model version `{s}`: this pr-guard knows {} and pr-guard:latest", known.join(", "))
            }),
        };
    }
    // A repo page (huggingface.co/<owner>/<repo>) names the repo; a longer URL there is a base like any other.
    let page = |p: &str| s.strip_prefix(p).filter(|r| r.trim_end_matches('/').split('@').next().is_some_and(|x| x.split('/').count() == 2));
    let hub = s.strip_prefix("hf:").or_else(|| page("https://huggingface.co/")).or_else(|| page("huggingface.co/"));
    if let Some(r) = hub {
        let (repo, revision) = r.trim_end_matches('/').split_once('@').unwrap_or((r.trim_end_matches('/'), "main"));
        anyhow::ensure!(repo.split('/').count() == 2 && !revision.is_empty(), "`{s}`: expected hf:<owner>/<repo>[@<revision>]");
        return Ok(Source::Hub { repo: repo.into(), revision: revision.into() });
    }
    if s.starts_with("https://") || s.starts_with("http://") {
        return Ok(Source::Url(s.trim_end_matches('/').into()));
    }
    if s.starts_with('.') || s.starts_with('/') || s.starts_with('~') || Path::new(s).is_dir() {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        return Ok(Source::Dir(s.strip_prefix("~/").map_or_else(|| PathBuf::from(s), |r| home.join(r))));
    }
    bail!("unknown model `{s}`: use pr-guard:<version>, pr-guard:latest, hf:<owner>/<repo>[@<revision>], an https:// URL or a directory")
}

fn release(r: (&'static str, &'static str, &'static str)) -> Source {
    Source::Release { version: r.0, revision: r.1, manifest: r.2 }
}

/// Where packages are kept: `$PR_GUARD_CACHE` (each CI wrapper keeps it in the CI cache), `$XDG_CACHE_HOME/pr-guard`,
/// `~/.cache/pr-guard`, or `%LOCALAPPDATA%\pr-guard` on Windows.
pub fn cache_root() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    env("PR_GUARD_CACHE")
        .or_else(|| env("XDG_CACHE_HOME").map(|d| d.join("pr-guard")))
        .or_else(|| env("HOME").map(|h| h.join(".cache/pr-guard")))
        .or_else(|| env("LOCALAPPDATA").map(|d| d.join("pr-guard")))
}

fn hub_base() -> String {
    // `$PR_GUARD_HF_ENDPOINT` points at a mirror (an internal proxy where the runners have no internet).
    std::env::var("PR_GUARD_HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".into()).trim_end_matches('/').to_string()
}

fn hex(d: impl AsRef<[u8]>) -> String {
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' }).collect()
}

/// A package is complete when its manifest is in place (written last) with the files it lists; a release's manifest
/// must also be the pinned one. The large files are not hashed again on every run; `fetch` checked them.
fn complete(dir: &Path, manifest: Option<&str>) -> bool {
    std::fs::read(dir.join("decision_ai.json")).is_ok_and(|m| manifest.is_none_or(|want| hex(Sha256::digest(m)) == want))
        && FILES.iter().all(|f| dir.join(f).is_file())
}

/// The directory holding `source`'s package, downloaded into the cache when missing; `quiet` keeps stderr for errors.
pub fn resolve(source: &Source, quiet: bool) -> Result<PathBuf> {
    let say = |m: String| {
        if !quiet {
            eprintln!("pr-guard: {m}");
        }
    };
    let root = || cache_root().context("no cache directory: set PR_GUARD_CACHE or HOME");
    let (dir, base, manifest, name) = match source {
        Source::Dir(d) => {
            anyhow::ensure!(d.join("decision_ai.json").is_file(), "no model package in {} (decision_ai.json missing)", d.display());
            return Ok(d.clone());
        }
        Source::Release { version, revision, manifest } => {
            (root()?.join(format!("pr-guard-{version}")), format!("{}/{REPO}/resolve/{revision}", hub_base()), Some(*manifest), format!("pr-guard:{version}"))
        }
        Source::Hub { repo, revision } => {
            // A branch or tag moves: the commit it points at now names the copy, so a new push is a new download.
            let sha = commit_of(repo, revision).with_context(|| format!("resolving {repo}@{revision}"))?;
            (root()?.join("hf").join(slug(repo)).join(&sha), format!("{}/{repo}/resolve/{sha}", hub_base()), None, format!("{repo}@{}", &sha[..7.min(sha.len())]))
        }
        Source::Url(u) => (root()?.join("url").join(&hex(Sha256::digest(u.as_bytes()))[..16]), u.clone(), None, u.clone()),
    };
    if complete(&dir, manifest) {
        return Ok(dir);
    }
    say(format!("downloading {name} into {}", dir.display()));
    let manifest_part = fetch(&format!("{base}/decision_ai.json"), &dir, "decision_ai.json", manifest)?;
    let sums: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_part)?).context("decision_ai.json is not JSON")?;
    for f in FILES {
        let want = sums["sha256"][f].as_str().with_context(|| format!("the manifest has no sha256 for {f}"))?;
        if dir.join(f).is_file() && hex(Sha256::digest(std::fs::read(dir.join(f))?)) == want {
            continue;
        }
        let t0 = std::time::Instant::now();
        let p = fetch(&format!("{base}/{f}"), &dir, f, Some(want))?;
        say(format!("{f}: {} MB in {:.0?}", std::fs::metadata(&p)?.len() / 1_000_000, t0.elapsed()));
    }
    std::fs::rename(&manifest_part, dir.join("decision_ai.json"))?;
    Ok(dir)
}

fn get(url: &str) -> Result<ureq::http::Response<ureq::Body>> {
    let mut req = ureq::get(url);
    // A private repo of your own; only sent to the Hub (or its mirror).
    if let Ok(t) = std::env::var("HF_TOKEN")
        && url.starts_with(&hub_base())
    {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    req.call().with_context(|| format!("downloading {url}"))
}

/// The commit a branch, tag or commit of a Hub repo points at.
fn commit_of(repo: &str, revision: &str) -> Result<String> {
    let v: serde_json::Value = get(&format!("{}/api/models/{repo}/revision/{revision}", hub_base()))?.into_body().read_json()?;
    v["sha"].as_str().map(str::to_string).context("the Hub answered without a commit")
}

/// One file, streamed to `<dir>/<name>.part` while hashing, renamed when the hash matches (`sha256: None` takes it as
/// it comes, for an unpinned manifest). The manifest stays `.part` for `resolve` to move last.
fn fetch(url: &str, dir: &Path, name: &str, sha256: Option<&str>) -> Result<PathBuf> {
    let dest = dir.join(name);
    std::fs::create_dir_all(dest.parent().unwrap_or(dir))?;
    let part = dest.with_file_name(format!("{}.part", dest.file_name().unwrap_or_default().to_string_lossy()));
    let mut body = get(url)?.into_body().into_reader();
    let mut out = std::io::BufWriter::new(std::fs::File::create(&part)?);
    let (mut hash, mut buf) = (Sha256::new(), vec![0u8; 1 << 16]);
    loop {
        let n = body.read(&mut buf).with_context(|| format!("downloading {url}"))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    out.flush()?;
    drop(out);
    let got = hex(hash.finalize());
    if let Some(want) = sha256
        && got != want
    {
        let _ = std::fs::remove_file(&part);
        bail!("{name} from {url} has sha256 {got}, expected {want}");
    }
    if name == "decision_ai.json" {
        return Ok(part);
    }
    std::fs::rename(&part, &dest)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_strings_name_a_release_the_latest_a_hub_repo_a_url_or_a_directory() {
        let v1 = parse("pr-guard:1").unwrap();
        assert!(matches!(v1, Source::Release { version: "1", .. }));
        assert_eq!(parse("pr-guard").unwrap(), v1);
        assert_eq!(parse("pr-guard:v1").unwrap(), v1);
        assert_eq!(parse(DEFAULT).unwrap(), v1);
        assert_eq!(parse("pr-guard:latest").unwrap(), Source::Hub { repo: REPO.into(), revision: "main".into() });
        assert!(parse("pr-guard:9").unwrap_err().to_string().contains("knows pr-guard:1"));
        assert_eq!(parse("hf:acme/guard@v2").unwrap(), Source::Hub { repo: "acme/guard".into(), revision: "v2".into() });
        assert_eq!(parse("https://huggingface.co/acme/guard").unwrap(), Source::Hub { repo: "acme/guard".into(), revision: "main".into() });
        assert!(parse("hf:guard").is_err());
        let files = "https://huggingface.co/acme/guard/resolve/main";
        assert_eq!(parse(&format!("{files}/")).unwrap(), Source::Url(files.into()));
        assert_eq!(parse("https://cdn.acme.dev/pr-guard/1/").unwrap(), Source::Url("https://cdn.acme.dev/pr-guard/1".into()));
        assert_eq!(parse("./models/guard").unwrap(), Source::Dir("./models/guard".into()));
        assert!(parse("something").is_err());
    }

    #[test]
    fn a_release_is_complete_only_with_its_pinned_manifest_and_files() {
        let dir = std::env::temp_dir().join(format!("pr-guard-pull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("onnx")).unwrap();
        assert!(!complete(&dir, None));
        std::fs::write(dir.join("decision_ai.json"), b"{}").unwrap();
        std::fs::write(dir.join("tokenizer.json"), b"{}").unwrap();
        std::fs::write(dir.join("onnx/model_8bit.onnx"), b"x").unwrap();
        // Any manifest completes an unpinned package; a release wants its own (an older model in the cache is pulled again).
        assert!(complete(&dir, None));
        assert!(!complete(&dir, Some(RELEASES[0].2)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
