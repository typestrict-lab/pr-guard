//! Where to publish (the third axis): the git host that holds the pull request, found from the `origin` remote, not from
//! the CI, because a Jenkins or CircleCI job can review a GitHub or Bitbucket pull request. With a token and a PR number
//! pr-guard keeps one sticky comment, the `pr-guard:*` labels and two commit statuses up to date; without them it only
//! prints, as before.
//!
//! The sticky comment also carries the verdict as JSON inside an HTML comment, so `pr-guard reviewers` can re-count the
//! approvals on a review event without the diff or the model.

mod bitbucket;
mod github;
mod gitlab;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ci::{Env, Provider};
use crate::policy::{Policy, Publish};
use crate::report::MARKER;
use crate::verdict::Verdict;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    GitHub,
    GitLab,
    Bitbucket,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::GitHub => "GitHub",
            Kind::GitLab => "GitLab",
            Kind::Bitbucket => "Bitbucket",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    Bearer(String),
    /// GitLab's `PRIVATE-TOKEN` header: personal, project and group access tokens.
    Private(String),
    /// Already base64 `user:secret` (Bitbucket API tokens and app passwords).
    Basic(String),
}

/// One pull request on one host, ready for API calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub kind: Kind,
    /// API root without a trailing slash: `https://api.github.com`, `https://gitlab.example.com/api/v4`.
    pub api: String,
    /// `owner/name`, `group/sub/project` or `workspace/repo`.
    pub repo: String,
    pub pr: String,
    pub auth: Auth,
    /// The CI job, linked from the statuses; Bitbucket requires a link, so it falls back to the pull request.
    pub link: Option<String>,
    /// Head commit as the CI knows it; `None` asks the API. GitHub's `GITHUB_SHA` and GitLab's merged-results
    /// `CI_COMMIT_SHA` are merge commits, which no status on the pull request would show, so those are not used.
    pub head: Option<String>,
}

/// Why nothing is published, for the log in CI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    NoHost,
    NoToken(Kind, &'static str),
    NoPr(Kind),
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Skip::NoHost => write!(f, "origin is not on GitHub, GitLab or Bitbucket (set PR_GUARD_HOST and PR_GUARD_API_URL for a self-hosted one)"),
            Skip::NoToken(k, hint) => write!(f, "no {} token ({hint})", k.name()),
            Skip::NoPr(k) => write!(f, "no {} pull request number (pass --pr)", k.name()),
        }
    }
}

/// The host, repository and token for `origin` (or, with no clone at hand, the CI's own server). `pr` overrides the
/// CI's pull request number.
pub fn target(env: &Env, origin: Option<&str>, pr: Option<&str>) -> Result<Target, Skip> {
    let g = |k: &str| env.get(k);
    let (host, path) = match origin.and_then(remote) {
        Some(r) => r,
        // A review workflow needs no checkout: the CI names its own server and repository.
        None => match (g("GITHUB_SERVER_URL"), g("GITHUB_REPOSITORY"), g("CI_SERVER_URL"), g("CI_PROJECT_PATH"), g("BITBUCKET_REPO_FULL_NAME")) {
            (Some(s), Some(r), ..) => (url_host(&s), r),
            (_, _, Some(s), Some(r), _) => (url_host(&s), r),
            (.., Some(r)) => ("bitbucket.org".into(), r),
            _ => return Err(Skip::NoHost),
        },
    };
    let same = |var: &str| g(var).is_some_and(|u| url_host(&u) == host);
    let forced = g("PR_GUARD_HOST").map(|h| h.to_ascii_lowercase());
    let kind = match forced.as_deref() {
        Some("github") => Kind::GitHub,
        Some("gitlab") => Kind::GitLab,
        Some("bitbucket") => Kind::Bitbucket,
        _ if host == "github.com" || same("GITHUB_SERVER_URL") => Kind::GitHub,
        _ if host == "gitlab.com" || same("CI_SERVER_URL") => Kind::GitLab,
        _ if host == "bitbucket.org" => Kind::Bitbucket,
        _ => return Err(Skip::NoHost),
    };
    let api = g("PR_GUARD_API_URL").unwrap_or_else(|| match kind {
        Kind::GitHub if host == "github.com" => "https://api.github.com".into(),
        // GitHub Enterprise Server: the runner exports its API root.
        Kind::GitHub => g("GITHUB_API_URL").filter(|_| same("GITHUB_SERVER_URL")).unwrap_or_else(|| format!("https://{host}/api/v3")),
        Kind::GitLab => g("CI_API_V4_URL").filter(|_| same("CI_SERVER_URL")).unwrap_or_else(|| format!("https://{host}/api/v4")),
        Kind::Bitbucket => "https://api.bitbucket.org/2.0".into(),
    });
    let api = api.trim_end_matches('/').to_string();
    // GitLab under a relative URL (`example.com/gitlab/group/project`) puts the prefix in the remote's path.
    let repo = match kind {
        Kind::GitLab => g("CI_PROJECT_PATH").filter(|_| same("CI_SERVER_URL")).unwrap_or(path),
        _ => path,
    };
    let auth = match kind {
        Kind::GitHub => g("GITHUB_TOKEN").or_else(|| g("GH_TOKEN")).map(Auth::Bearer).ok_or(Skip::NoToken(kind, "set GITHUB_TOKEN"))?,
        Kind::GitLab => match g("GITLAB_TOKEN") {
            Some(t) => Auth::Private(t),
            // The job token cannot write notes, labels, statuses or approval rules: say so instead of failing on a 403.
            None if g("CI_JOB_TOKEN").is_some() => {
                return Err(Skip::NoToken(kind, "CI_JOB_TOKEN cannot write notes or statuses; set GITLAB_TOKEN to a project access token with the api scope"));
            }
            None => return Err(Skip::NoToken(kind, "set GITLAB_TOKEN")),
        },
        Kind::Bitbucket => match (g("BITBUCKET_TOKEN"), g("BITBUCKET_USERNAME")) {
            (Some(t), Some(u)) => Auth::Basic(base64(&format!("{u}:{t}"))),
            (Some(t), None) => Auth::Bearer(t),
            (None, _) => return Err(Skip::NoToken(kind, "set BITBUCKET_TOKEN to a repository access token")),
        },
    };
    let event = g("GITHUB_EVENT_PATH").and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let event_pr = |key: &str| event.as_ref().and_then(|e| e.pointer(&format!("/pull_request/{key}")).cloned());
    let ci = crate::ci::detect(env);
    let on = |p: Provider| ci.as_ref().is_some_and(|c| c.provider == p);
    let pr = pr.map(String::from).or_else(|| ci.as_ref().and_then(|c| c.pr.clone())).or_else(|| event_pr("number").map(|n| n.to_string()));
    let pr = pr.ok_or(Skip::NoPr(kind))?;
    let head = if on(Provider::GitHub) {
        event_pr("head").and_then(|h| h.get("sha")?.as_str().map(String::from))
    } else if on(Provider::GitLab) {
        g("CI_MERGE_REQUEST_SOURCE_BRANCH_SHA").or_else(|| g("CI_COMMIT_SHA"))
    } else if on(Provider::Bitbucket) {
        g("BITBUCKET_COMMIT")
    } else {
        None
    };
    let link = match (g("GITHUB_SERVER_URL"), g("GITHUB_REPOSITORY"), g("GITHUB_RUN_ID")) {
        (Some(s), Some(r), Some(id)) => Some(format!("{s}/{r}/actions/runs/{id}")),
        _ => g("CI_JOB_URL").or_else(|| Some(format!("{}/pipelines/results/{}", g("BITBUCKET_GIT_HTTP_ORIGIN")?, g("BITBUCKET_BUILD_NUMBER")?))),
    };
    Ok(Target { kind, api, repo, pr, auth, link, head })
}

/// `(host, path)` from an https, ssh or scp-like remote: the host lowercased without user or port, the path without
/// `.git`. CI clones embed a token as the user (`https://gitlab-ci-token:…@host/…`); it is dropped here.
pub fn remote(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (authority, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => url.split_once(':')?,
    };
    let host = authority.rsplit('@').next()?.split(':').next()?.to_ascii_lowercase();
    let path = path.trim_matches('/').trim_end_matches(".git").trim_matches('/');
    (!host.is_empty() && path.contains('/')).then(|| (host, path.to_string()))
}

fn url_host(url: &str) -> String {
    remote(&format!("{}/x/y", url.trim_end_matches('/'))).map(|(h, _)| h).unwrap_or_default()
}

// --- HTTP, behind a trait so the tests record requests instead of reaching a host ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

pub trait Http {
    fn send(&self, req: &Request) -> anyhow::Result<Response>;
}

/// The real client: blocking ureq with rustls, statuses as values (the hosts' 4xx bodies explain the error).
pub struct Ureq(ureq::Agent);

impl Ureq {
    pub fn new() -> Ureq {
        let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(std::time::Duration::from_secs(30))).build();
        Ureq(config.into())
    }
}

impl Default for Ureq {
    fn default() -> Self {
        Ureq::new()
    }
}

impl Http for Ureq {
    fn send(&self, req: &Request) -> anyhow::Result<Response> {
        fn with<B>(mut b: ureq::RequestBuilder<B>, h: &[(&'static str, String)]) -> ureq::RequestBuilder<B> {
            for (k, v) in h {
                b = b.header(*k, v);
            }
            b
        }
        let body = req.body.as_ref().map(Value::to_string).unwrap_or_default();
        let sent = match req.method {
            Method::Get => with(self.0.get(&req.url), &req.headers).call(),
            Method::Delete => with(self.0.delete(&req.url), &req.headers).call(),
            m => {
                let b = match m {
                    Method::Post => self.0.post(&req.url),
                    Method::Put => self.0.put(&req.url),
                    _ => self.0.patch(&req.url),
                };
                with(b, &req.headers).header("Content-Type", "application/json").send(body.as_bytes())
            }
        };
        // The URL carries no secret (tokens go in headers), so it can be in the error.
        let mut resp = sent.map_err(|e| anyhow::anyhow!("{:?} {}: {e}", req.method, req.url))?;
        Ok(Response { status: resp.status().as_u16(), body: resp.body_mut().read_to_string()? })
    }
}

/// One host's REST calls, in the shape the flows below need. Every call is about `Target.pr`.
pub(crate) struct Client<'a> {
    pub http: &'a dyn Http,
    pub t: &'a Target,
}

impl Client<'_> {
    fn call(&self, method: Method, url: String, body: Option<Value>) -> anyhow::Result<Value> {
        let auth = match &self.t.auth {
            Auth::Bearer(t) => ("Authorization", format!("Bearer {t}")),
            Auth::Private(t) => ("PRIVATE-TOKEN", t.clone()),
            Auth::Basic(b) => ("Authorization", format!("Basic {b}")),
        };
        let mut headers = vec![auth, ("User-Agent", format!("pr-guard/{}", env!("CARGO_PKG_VERSION"))), ("Accept", "application/json".into())];
        if self.t.kind == Kind::GitHub {
            headers[2].1 = "application/vnd.github+json".into();
            headers.push(("X-GitHub-Api-Version", "2022-11-28".into()));
        }
        let r = self.http.send(&Request { method, url: url.clone(), headers, body })?;
        if !(200..300).contains(&r.status) {
            let msg: String = r.body.chars().take(300).collect();
            anyhow::bail!("{} {method:?} {url}: HTTP {}: {msg}", self.t.kind.name(), r.status);
        }
        Ok(if r.body.trim().is_empty() { Value::Null } else { serde_json::from_str(&r.body)? })
    }

    pub fn get(&self, url: String) -> anyhow::Result<Value> {
        self.call(Method::Get, url, None)
    }
    pub fn post(&self, url: String, body: Value) -> anyhow::Result<Value> {
        self.call(Method::Post, url, Some(body))
    }
    pub fn put(&self, url: String, body: Value) -> anyhow::Result<Value> {
        self.call(Method::Put, url, Some(body))
    }
    pub fn patch(&self, url: String, body: Value) -> anyhow::Result<Value> {
        self.call(Method::Patch, url, Some(body))
    }
    pub fn delete(&self, url: String) -> anyhow::Result<Value> {
        self.call(Method::Delete, url, None)
    }

    /// Pages of `per_page=100` until a short one (GitHub and GitLab).
    pub fn pages(&self, url: &str) -> anyhow::Result<Vec<Value>> {
        let sep = if url.contains('?') { '&' } else { '?' };
        let mut all = Vec::new();
        for page in 1.. {
            let Value::Array(items) = self.get(format!("{url}{sep}per_page=100&page={page}"))? else { anyhow::bail!("{url}: not a list") };
            let n = items.len();
            all.extend(items);
            if n < 100 || page == 50 {
                break;
            }
        }
        Ok(all)
    }

    fn api(&self) -> &dyn Api {
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Success,
    Failure,
    /// Not there yet (approvals missing): pending on GitHub and GitLab; failed on Bitbucket, whose merge check only
    /// counts builds that passed.
    Waiting,
}

/// One status for the whole verdict: failed when the policy blocks, pending until the approvals it asks for, else passed.
pub const STATUS: &str = "pr-guard";

pub(crate) trait Api {
    /// The token's own account, whose comments are the only ones trusted.
    fn me(&self) -> anyhow::Result<String>;
    fn comments(&self) -> anyhow::Result<Vec<Comment>>;
    fn create_comment(&self, body: &str) -> anyhow::Result<()>;
    fn update_comment(&self, id: &str, body: &str) -> anyhow::Result<()>;
    /// `None` when the host has no pull request labels (Bitbucket).
    fn labels(&self) -> anyhow::Result<Option<Vec<String>>>;
    fn set_labels(&self, add: &[String], remove: &[String]) -> anyhow::Result<()>;
    fn status(&self, sha: &str, name: &str, state: State, description: &str) -> anyhow::Result<()>;
    fn head(&self) -> anyhow::Result<String>;
    fn approvals(&self) -> anyhow::Result<u32>;
    /// Makes the host itself require `n` approvals; `Ok(false)` when it has no such setting and a status must do it.
    fn require_approvals(&self, _n: u8) -> anyhow::Result<bool> {
        Ok(false)
    }
}

impl Api for Client<'_> {
    fn me(&self) -> anyhow::Result<String> {
        match self.t.kind {
            Kind::GitHub => github::me(self),
            Kind::GitLab => gitlab::me(self),
            Kind::Bitbucket => bitbucket::me(self),
        }
    }
    fn comments(&self) -> anyhow::Result<Vec<Comment>> {
        match self.t.kind {
            Kind::GitHub => github::comments(self),
            Kind::GitLab => gitlab::comments(self),
            Kind::Bitbucket => bitbucket::comments(self),
        }
    }
    fn create_comment(&self, body: &str) -> anyhow::Result<()> {
        match self.t.kind {
            Kind::GitHub => github::create_comment(self, body),
            Kind::GitLab => gitlab::create_comment(self, body),
            Kind::Bitbucket => bitbucket::create_comment(self, body),
        }
    }
    fn update_comment(&self, id: &str, body: &str) -> anyhow::Result<()> {
        match self.t.kind {
            Kind::GitHub => github::update_comment(self, id, body),
            Kind::GitLab => gitlab::update_comment(self, id, body),
            Kind::Bitbucket => bitbucket::update_comment(self, id, body),
        }
    }
    fn labels(&self) -> anyhow::Result<Option<Vec<String>>> {
        match self.t.kind {
            Kind::GitHub => github::labels(self).map(Some),
            Kind::GitLab => gitlab::labels(self).map(Some),
            Kind::Bitbucket => Ok(None),
        }
    }
    fn set_labels(&self, add: &[String], remove: &[String]) -> anyhow::Result<()> {
        match self.t.kind {
            Kind::GitHub => github::set_labels(self, add, remove),
            Kind::GitLab => gitlab::set_labels(self, add, remove),
            Kind::Bitbucket => Ok(()),
        }
    }
    fn status(&self, sha: &str, name: &str, state: State, description: &str) -> anyhow::Result<()> {
        match self.t.kind {
            Kind::GitHub => github::status(self, sha, name, state, description),
            Kind::GitLab => gitlab::status(self, sha, name, state, description),
            Kind::Bitbucket => bitbucket::status(self, sha, name, state, description),
        }
    }
    fn head(&self) -> anyhow::Result<String> {
        match self.t.kind {
            Kind::GitHub => github::head(self),
            Kind::GitLab => gitlab::head(self),
            Kind::Bitbucket => bitbucket::head(self),
        }
    }
    fn approvals(&self) -> anyhow::Result<u32> {
        match self.t.kind {
            Kind::GitHub => github::approvals(self),
            Kind::GitLab => gitlab::approvals(self),
            Kind::Bitbucket => bitbucket::approvals(self),
        }
    }
    fn require_approvals(&self, n: u8) -> anyhow::Result<bool> {
        match self.t.kind {
            Kind::GitLab => gitlab::require_approvals(self, n),
            _ => Ok(false),
        }
    }
}

// --- The verdict kept in the sticky comment ---

const STORED: &str = "<!-- pr-guard:verdict ";

/// What `pr-guard reviewers` needs from the last check to set the status again, and the commit it was for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub head: String,
    pub reviewers: u8,
    pub shadow: bool,
    pub blocked: bool,
    pub skipped: bool,
    /// Why the policy blocks, for the status a review event sets again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

impl Stored {
    pub fn of(v: &Verdict, head: &str) -> Stored {
        let why = v.blocked.then(|| v.findings.first().map_or("policy".into(), |f| f.message.clone()));
        Stored { head: head.into(), reviewers: v.reviewers, shadow: v.shadow, blocked: v.blocked, skipped: v.skipped, why }
    }

    /// Approvals the host must see before `pr-guard` passes: none in shadow mode or when skipped.
    pub fn needed(&self) -> u8 {
        if self.shadow || self.skipped { 0 } else { self.reviewers }
    }
}

/// The report followed by the verdict as JSON in an HTML comment (invisible once rendered). JSON has `>` only inside
/// strings, where `>` means the same, so a finding's message cannot close the comment early.
pub fn sticky(markdown: &str, s: &Stored) -> String {
    let json = serde_json::to_string(s).expect("a stored verdict always serializes").replace('>', "\\u003e");
    format!("{}\n{STORED}{json} -->\n", markdown.trim_end())
}

pub fn stored(body: &str) -> Option<Stored> {
    let rest = &body[body.find(STORED)? + STORED.len()..];
    serde_json::from_str(rest[..rest.find("-->")?].trim()).ok()
}

/// Our sticky comment: the first with the marker written by `me`. Someone else's copy of the marker (a quote, or a
/// forged verdict asking for no reviewers) is never read nor edited.
fn find_sticky(api: &dyn Api, me: &str) -> anyhow::Result<Option<Comment>> {
    Ok(api.comments()?.into_iter().find(|c| c.author == me && c.body.contains(MARKER)))
}

/// `PR_GUARD_BOT` names the account when the token cannot say who it is (GitHub's `GITHUB_TOKEN` is an app
/// installation, which `/user` refuses; on Actions that account is `github-actions[bot]`).
fn whoami(api: &dyn Api, env: &Env) -> anyhow::Result<String> {
    if let Some(bot) = env.get("PR_GUARD_BOT") {
        return Ok(bot);
    }
    api.me().or_else(|e| match env.get("GITHUB_ACTIONS") {
        Some(_) => Ok("github-actions[bot]".into()),
        None => Err(e.context("cannot tell which account the token is (set PR_GUARD_BOT)")),
    })
}

/// A host label for a verdict label: GitHub caps names at 50 characters and GitLab splits its label lists on commas,
/// so a change type keeps its words before the first comma, as a slug.
pub fn host_label(label: &str) -> String {
    let Some(t) = label.strip_prefix("pr-guard:type-") else { return label.to_string() };
    let words = t.split(',').next().unwrap_or(t).to_ascii_lowercase();
    let slug: Vec<&str> = words.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let mut s = format!("pr-guard:type-{}", slug.join("-"));
    while s.len() > 50 {
        s.truncate(s.rfind('-').unwrap_or(50));
    }
    s
}

/// The labels the verdict wants on the pull request, as the host spells them.
pub fn wanted_labels(v: &Verdict) -> Vec<String> {
    let mut l: Vec<String> = v.labels.iter().map(|l| host_label(l)).collect();
    if v.blocked {
        l.push("pr-guard:blocked".into());
    }
    l
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).chain(['…']).collect() }
}

/// The status of the head commit from the verdict and, when `count`, the approvals the host has: failed when the policy
/// blocks (never in shadow mode, where `blocked` is false), pending while approvals are missing, else passed. GitLab
/// gets the number as the merge request's approval rule, which it enforces itself. A verdict for an older commit waits
/// for the check of the new one, so an approval of code nobody re-checked does not pass it.
fn status_of(api: &dyn Api, s: &Stored, head: &str, count: bool) -> anyhow::Result<(State, String)> {
    let plural = |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let who = plural(u32::from(s.reviewers), "reviewer", "reviewers");
    if s.skipped {
        return Ok((State::Success, "Skipped by label".into()));
    }
    if s.head.is_empty() || !(s.head.starts_with(head) || head.starts_with(&s.head)) {
        // Bitbucket's API gives 12-character hashes, the pipeline full ones.
        return Ok((State::Waiting, "Waiting for pr-guard to check the new commits".into()));
    }
    if s.blocked {
        return Ok((State::Failure, format!("Blocked: {}", s.why.as_deref().unwrap_or("policy"))));
    }
    if !count {
        return Ok((State::Success, if s.shadow { format!("Shadow mode: would need {who}") } else { format!("Needs {who}") }));
    }
    let need = s.needed();
    let by_rule = api.require_approvals(need)?;
    if s.shadow {
        return Ok((State::Success, format!("Shadow mode: would need {who}")));
    }
    if need == 0 {
        return Ok((State::Success, "Passed, no reviewer needed".into()));
    }
    if by_rule {
        return Ok((State::Success, format!("Needs {who}, required by the merge request's approval rule")));
    }
    let have = api.approvals()?;
    let of = format!("{have} of {need} {}", if need == 1 { "approval" } else { "approvals" });
    Ok(if have >= u32::from(need) { (State::Success, format!("Passed, {of}")) } else { (State::Waiting, format!("Needs {who}: {of}")) })
}

/// Everything `check` publishes. Each part runs even when another fails (a token without label rights still comments),
/// and failures come back as messages: publishing is reporting, so it never changes the verdict's exit code.
pub(crate) fn publish_with(api: &dyn Api, env: &Env, t: &Target, policy: &Policy, v: &Verdict, markdown: &str) -> Vec<String> {
    let Publish { comment, labels, status, reviewers } = policy.publish;
    let mut errors = Vec::new();
    let mut note = |what: &str, r: anyhow::Result<()>| {
        if let Err(e) = r {
            errors.push(format!("{what}: {e:#}"));
        }
    };
    let head = match t.head.clone().map_or_else(|| api.head(), Ok) {
        Ok(h) => h,
        Err(e) => {
            note("head commit", Err(e));
            String::new()
        }
    };
    let stored = Stored::of(v, &head);
    if comment {
        note("comment", (|| {
            let body = sticky(markdown, &stored);
            let me = whoami(api, env).ok();
            match me.map(|me| find_sticky(api, &me)).transpose()?.flatten() {
                Some(c) if c.body == body => Ok(()),
                Some(c) => api.update_comment(&c.id, &body),
                None => api.create_comment(&body),
            }
        })());
    }
    if labels {
        note("labels", (|| {
            let Some(current) = api.labels()? else { return Ok(()) };
            let want = wanted_labels(v);
            // Only our own labels move; the skip label is the people's switch and stays.
            let remove: Vec<String> =
                current.iter().filter(|l| l.starts_with("pr-guard:") && **l != policy.skip_label && !want.contains(l)).cloned().collect();
            let add: Vec<String> = want.into_iter().filter(|l| !current.contains(l)).collect();
            if add.is_empty() && remove.is_empty() { Ok(()) } else { api.set_labels(&add, &remove) }
        })());
    }
    if !head.is_empty() && status {
        note("status", status_of(api, &stored, &head, reviewers).and_then(|(state, d)| api.status(&head, STATUS, state, &clip(&d, 140))));
    } else if reviewers {
        // No status to wait on: only GitLab's approval rule can still hold the merge.
        note("required reviewers", api.require_approvals(stored.needed()).map(drop));
    }
    errors
}

pub fn publish(http: &dyn Http, env: &Env, t: &Target, policy: &Policy, v: &Verdict, markdown: &str) -> Vec<String> {
    let c = Client { http, t };
    publish_with(c.api(), env, t, policy, v, markdown)
}

/// The labels on the pull request (none on Bitbucket).
pub fn labels(http: &dyn Http, t: &Target) -> anyhow::Result<Vec<String>> {
    Client { http, t }.api().labels().map(Option::unwrap_or_default)
}

/// `pr-guard reviewers`: re-count the approvals against the verdict in the sticky comment. `None` when there is no
/// verdict yet (the check has not run on this pull request).
pub(crate) fn reviewers_with(api: &dyn Api, env: &Env, t: &Target) -> anyhow::Result<Option<String>> {
    let me = whoami(api, env)?;
    let Some(s) = find_sticky(api, &me)?.and_then(|c| stored(&c.body)) else { return Ok(None) };
    let head = t.head.clone().map_or_else(|| api.head(), Ok)?;
    let (state, d) = status_of(api, &s, &head, true)?;
    api.status(&head, STATUS, state, &clip(&d, 140))?;
    Ok(Some(d))
}

pub fn reviewers(http: &dyn Http, env: &Env, t: &Target) -> anyhow::Result<Option<String>> {
    let c = Client { http, t };
    reviewers_with(c.api(), env, t)
}

/// Path segment or query value: everything but the unreserved characters is percent-encoded (GitLab's
/// `group%2Fproject` ids, label names with `:` and spaces).
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn base64(s: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in s.as_bytes().chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// A field as a string whether the host sends it as a number or a string (ids).
fn id(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests;
