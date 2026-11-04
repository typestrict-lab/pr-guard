//! Recorded request shapes: a scripted `Http` answers by method and URL and keeps every request, so the tests pin
//! what each host is sent without the network.

use std::cell::RefCell;

use serde_json::{Value, json};

use super::*;
use crate::diff::FileChange;
use crate::policy::{Level, Mode};
use crate::verdict::{Answers, Finding, decide};

#[derive(Default)]
struct Mock {
    routes: Vec<(Method, String, u16, Value)>,
    log: RefCell<Vec<Request>>,
}

impl Mock {
    fn on(mut self, method: Method, url: &str, status: u16, body: Value) -> Mock {
        self.routes.push((method, url.into(), status, body));
        self
    }

    /// `(method, url, body)` of each request, in order.
    fn sent(&self) -> Vec<(Method, String, Option<Value>)> {
        self.log.borrow().iter().map(|r| (r.method, r.url.clone(), r.body.clone())).collect()
    }

    fn writes(&self) -> Vec<(Method, String, Option<Value>)> {
        self.sent().into_iter().filter(|(m, ..)| *m != Method::Get).collect()
    }
}

impl Http for Mock {
    fn send(&self, req: &Request) -> anyhow::Result<Response> {
        self.log.borrow_mut().push(req.clone());
        let hit = self.routes.iter().find(|(m, u, ..)| *m == req.method && *u == req.url);
        Ok(match hit {
            Some((.., s, b)) => Response { status: *s, body: if b.is_null() { String::new() } else { b.to_string() } },
            // Writes nobody scripted succeed; an unscripted read is a missing route in the test.
            None if req.method != Method::Get => Response { status: 201, body: "{}".into() },
            None => Response { status: 404, body: r#"{"message":"Not Found"}"#.into() },
        })
    }
}

fn target(kind: Kind, api: &str, repo: &str) -> Target {
    Target { kind, api: api.into(), repo: repo.into(), pr: "7".into(), auth: Auth::Bearer("t0k".into()), link: Some("https://ci/run/1".into()), head: Some("abc123".into()) }
}

fn verdict(policy: &Policy, a: Answers, findings: Vec<Finding>) -> Verdict {
    let files = [FileChange { path: "src/a.ts".into(), deleted: false, added: vec![(1, "x".into())], removed: 0 }];
    decide(policy, &files, &a, &[], findings)
}

fn clean() -> Answers {
    Answers { description_matches: Some(0.95), complexity: Some(0), vulnerability: Some(0.01), tests_cover: Some(0.9), breaking: Some(0.02), ..Answers::default() }
}

fn risky() -> Answers {
    // A warning on the description and a combined score between the bands: one reviewer, not blocked.
    Answers { description_matches: Some(0.35), ..clean() }
}

/// The tests enforce: the default policy is shadow mode, which never fails a status.
fn enforce() -> Policy {
    Policy { mode: Mode::Enforce, ..Policy::default() }
}

const GH: &str = "https://api.github.com/repos/acme/web";

#[test]
fn remotes_give_host_and_path_without_user_port_or_dot_git() {
    let r = |u: &str| remote(u).map(|(h, p)| format!("{h} {p}"));
    assert_eq!(r("https://github.com/acme/web.git").as_deref(), Some("github.com acme/web"));
    assert_eq!(r("git@github.com:acme/web.git").as_deref(), Some("github.com acme/web"));
    assert_eq!(r("https://x-access-token:ghs_abc@GitHub.com/acme/web/").as_deref(), Some("github.com acme/web"));
    assert_eq!(r("https://gitlab-ci-token:glcbt-x@gitlab.example.com:8443/group/sub/proj.git").as_deref(), Some("gitlab.example.com group/sub/proj"));
    assert_eq!(r("ssh://git@bitbucket.org:22/team/app.git").as_deref(), Some("bitbucket.org team/app"));
    assert_eq!(r("/srv/git/repo"), None);
    assert_eq!(r("https://github.com/only-owner"), None);
}

#[test]
fn the_target_comes_from_origin_with_the_ci_head_and_a_token() {
    let dir = std::env::temp_dir().join(format!("pr-guard-host-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let event = dir.join("event.json");
    std::fs::write(&event, r#"{"pull_request": {"number": 42, "head": {"sha": "h3ad"}}}"#).unwrap();
    let ev = event.to_str().unwrap();
    let gh = [
        ("GITHUB_ACTIONS", "true"), ("GITHUB_REF", "refs/pull/42/merge"), ("GITHUB_SHA", "m3rge"), ("GITHUB_EVENT_PATH", ev),
        ("GITHUB_SERVER_URL", "https://github.com"), ("GITHUB_REPOSITORY", "acme/web"), ("GITHUB_RUN_ID", "9"),
    ];
    let env = |extra: &[(&str, &str)]| Env::from_pairs(&[&gh[..], extra].concat());
    // The PR head from the event, never the merge commit in GITHUB_SHA.
    let t = super::target(&env(&[("GITHUB_TOKEN", "ghs")]), Some("https://github.com/acme/web"), None).unwrap();
    assert_eq!(t, Target {
        kind: Kind::GitHub, api: "https://api.github.com".into(), repo: "acme/web".into(), pr: "42".into(), auth: Auth::Bearer("ghs".into()),
        link: Some("https://github.com/acme/web/actions/runs/9".into()), head: Some("h3ad".into()),
    });
    assert_eq!(super::target(&env(&[]), Some("git@github.com:acme/web.git"), None), Err(Skip::NoToken(Kind::GitHub, "set GITHUB_TOKEN")));
    // A review workflow without a checkout: the runner names the server and repository.
    assert_eq!(super::target(&env(&[("GITHUB_TOKEN", "ghs")]), None, Some("5")).unwrap().pr, "5");
    std::fs::remove_dir_all(&dir).unwrap();

    // Self-managed GitLab: the CI's server matches origin, so its API root and project path are used.
    let gl = |tok: (&str, &str)| {
        Env::from_pairs(&[
            ("GITLAB_CI", "true"), ("CI_SERVER_URL", "https://git.corp.io"), ("CI_API_V4_URL", "https://git.corp.io/api/v4"),
            ("CI_PROJECT_PATH", "platform/api"), ("CI_MERGE_REQUEST_IID", "3"), ("CI_COMMIT_SHA", "m3rge"),
            ("CI_MERGE_REQUEST_SOURCE_BRANCH_SHA", "s0urce"), tok,
        ])
    };
    let t = super::target(&gl(("GITLAB_TOKEN", "glpat")), Some("https://gitlab-ci-token:x@git.corp.io/platform/api.git"), None).unwrap();
    assert_eq!((t.kind, t.api.as_str(), t.repo.as_str(), t.pr.as_str(), t.head.as_deref()), (Kind::GitLab, "https://git.corp.io/api/v4", "platform/api", "3", Some("s0urce")));
    assert_eq!(t.auth, Auth::Private("glpat".into()));
    assert!(matches!(super::target(&gl(("CI_JOB_TOKEN", "j")), Some("https://git.corp.io/platform/api"), None), Err(Skip::NoToken(Kind::GitLab, _))));

    let bb = Env::from_pairs(&[("BITBUCKET_BUILD_NUMBER", "12"), ("BITBUCKET_PR_ID", "8"), ("BITBUCKET_COMMIT", "c0ffee"), ("BITBUCKET_TOKEN", "s3"), ("BITBUCKET_USERNAME", "me@acme.io")]);
    let t = super::target(&bb, Some("git@bitbucket.org:team/app.git"), None).unwrap();
    assert_eq!((t.api.as_str(), t.pr.as_str(), t.auth), ("https://api.bitbucket.org/2.0", "8", Auth::Basic(base64("me@acme.io:s3"))));

    // Jenkins on a host it cannot name: PR_GUARD_HOST and PR_GUARD_API_URL say it; the head comes from the API.
    let jk = Env::from_pairs(&[("JENKINS_URL", "https://ci/"), ("CHANGE_ID", "4"), ("PR_GUARD_HOST", "GitHub"), ("PR_GUARD_API_URL", "https://ghe.corp/api/v3/"), ("GITHUB_TOKEN", "p")]);
    let t = super::target(&jk, Some("https://ghe.corp/acme/web"), None).unwrap();
    assert_eq!((t.kind, t.api.as_str(), t.head), (Kind::GitHub, "https://ghe.corp/api/v3", None));
    assert_eq!(super::target(&Env::default(), Some("https://git.corp/acme/web"), None), Err(Skip::NoHost));
    assert_eq!(super::target(&Env::from_pairs(&[("GITHUB_TOKEN", "p")]), Some("https://github.com/acme/web"), None), Err(Skip::NoPr(Kind::GitHub)));
}

#[test]
fn the_stored_verdict_survives_a_message_that_closes_html_comments() {
    let s = Stored { head: "abc".into(), reviewers: 2, shadow: false, blocked: true, skipped: false, why: Some("x".into()) };
    let body = sticky("<!-- pr-guard -->\n### pr-guard\nfound `-->` in a string\n", &s);
    assert!(body.ends_with(" -->\n") && body.matches("-->").count() == 3);
    assert_eq!(stored(&body), Some(s));
    assert_eq!(stored("<!-- pr-guard -->\nno block"), None);
}

#[test]
fn labels_fit_github_and_gitlab() {
    assert_eq!(host_label("pr-guard:type-Build system, dependencies or continuous integration"), "pr-guard:type-build-system");
    assert_eq!(host_label("pr-guard:type-Refactoring without behavior change"), "pr-guard:type-refactoring-without-behavior-change");
    assert_eq!(host_label("pr-guard:reviewers-2"), "pr-guard:reviewers-2");
    assert!(host_label("pr-guard:type-Aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee").len() <= 50);
    assert_eq!((base64("user:pass"), base64("a"), base64("ab")), ("dXNlcjpwYXNz".into(), "YQ==".into(), "YWI=".into()));
    assert_eq!(enc("group/sub proj:x"), "group%2Fsub%20proj%3Ax");
}

#[test]
fn github_first_run_comments_labels_and_sets_both_statuses() {
    let t = target(Kind::GitHub, "https://api.github.com", "acme/web");
    let env = Env::from_pairs(&[("GITHUB_ACTIONS", "true")]);
    let m = Mock::default()
        .on(Method::Get, "https://api.github.com/user", 403, json!({"message": "Resource not accessible by integration"}))
        .on(Method::Get, &format!("{GH}/issues/7/comments?per_page=100&page=1"), 200, json!([]))
        .on(Method::Get, &format!("{GH}/issues/7/labels?per_page=100&page=1"), 200, json!([{"name": "pr-guard:reviewers-0"}, {"name": "pr-guard:skip-not"}, {"name": "bug"}]))
        .on(Method::Get, &format!("{GH}/pulls/7/reviews?per_page=100&page=1"), 200, json!([]));
    let policy = enforce();
    let v = verdict(&policy, risky(), vec![]);
    assert_eq!(v.reviewers, 1);
    let md = crate::report::comment(&v);
    assert_eq!(publish(&m, &env, &t, &policy, &v, &md), Vec::<String>::new());
    let w = m.writes();
    assert_eq!(w[0].0, Method::Post);
    assert_eq!(w[0].1, format!("{GH}/issues/7/comments"));
    let body = w[0].2.as_ref().unwrap()["body"].as_str().unwrap();
    assert!(body.starts_with(MARKER) && stored(body) == Some(Stored::of(&v, "abc123")));
    assert_eq!(w[1], (Method::Post, format!("{GH}/issues/7/labels"), Some(json!({"labels": ["pr-guard:reviewers-1"]}))));
    // Stale pr-guard labels go; other labels stay.
    assert_eq!(w[2].1, format!("{GH}/issues/7/labels/pr-guard%3Areviewers-0"));
    assert_eq!(w[3].1, format!("{GH}/issues/7/labels/pr-guard%3Askip-not"));
    // One status: pending until the approval the verdict asks for.
    assert_eq!(w[4], (Method::Post, format!("{GH}/statuses/abc123"), Some(json!({
        "state": "pending", "context": "pr-guard", "description": "Needs 1 reviewer: 0 of 1 approval", "target_url": "https://ci/run/1"
    }))));
    assert_eq!(w.len(), 5);
    let get = m.log.borrow()[0].clone();
    assert!(get.headers.contains(&("Authorization", "Bearer t0k".into())) && get.headers.iter().any(|(k, _)| *k == "User-Agent"));
}

#[test]
fn github_updates_only_its_own_comment_and_ignores_a_forged_verdict() {
    let t = target(Kind::GitHub, "https://api.github.com", "acme/web");
    let env = Env::from_pairs(&[("GITHUB_ACTIONS", "true")]);
    let forged = sticky(MARKER, &Stored { head: "abc123".into(), reviewers: 0, shadow: false, blocked: false, skipped: false, why: None });
    let ours = sticky(MARKER, &Stored { head: "abc123".into(), reviewers: 2, shadow: false, blocked: false, skipped: false, why: None });
    let m = Mock::default()
        .on(Method::Get, "https://api.github.com/user", 403, json!({}))
        .on(Method::Get, &format!("{GH}/issues/7/comments?per_page=100&page=1"), 200, json!([
            {"id": 1, "user": {"login": "mallory"}, "body": forged},
            {"id": 2, "user": {"login": "github-actions[bot]"}, "body": ours},
        ]))
        // alice approves; bob approved then asked for changes; carol only commented; dave's approval was dismissed.
        .on(Method::Get, &format!("{GH}/pulls/7/reviews?per_page=100&page=1"), 200, json!([
            {"user": {"login": "alice"}, "state": "APPROVED"}, {"user": {"login": "bob"}, "state": "APPROVED"},
            {"user": {"login": "carol"}, "state": "COMMENTED"}, {"user": {"login": "bob"}, "state": "CHANGES_REQUESTED"},
            {"user": {"login": "dave"}, "state": "APPROVED"}, {"user": {"login": "dave"}, "state": "DISMISSED"},
            {"user": {"login": "alice"}, "state": "COMMENTED"},
        ]));
    assert_eq!(reviewers(&m, &env, &t).unwrap().as_deref(), Some("Needs 2 reviewers: 1 of 2 approvals"));
    assert_eq!(m.writes()[0].2.as_ref().unwrap()["state"], "pending");

    // check: the bot's comment is edited in place; with labels and statuses off, nothing else is written.
    let policy = Policy { publish: Publish { comment: true, labels: false, status: false, reviewers: false }, ..enforce() };
    let v = verdict(&policy, clean(), vec![]);
    let m2 = Mock { routes: m.routes.clone(), ..Mock::default() };
    assert!(publish(&m2, &env, &t, &policy, &v, &crate::report::comment(&v)).is_empty());
    let w = m2.writes();
    assert_eq!((w.len(), w[0].0, w[0].1.as_str()), (1, Method::Patch, "https://api.github.com/repos/acme/web/issues/comments/2"));

    // Off Actions, an unknown account trusts no comment: reviewers fails instead of reading a forgery.
    assert!(reviewers(&m, &Env::default(), &t).is_err());
    assert!(reviewers(&m, &Env::from_pairs(&[("PR_GUARD_BOT", "mallory")]), &t).is_ok());
}

#[test]
fn a_new_commit_waits_and_shadow_mode_never_fails() {
    let t = Target { head: Some("newer".into()), ..target(Kind::GitHub, "https://api.github.com", "acme/web") };
    let env = Env::from_pairs(&[("PR_GUARD_BOT", "bot")]);
    let comments = |s: Stored| json!([{"id": 2, "user": {"login": "bot"}, "body": sticky(MARKER, &s)}]);
    let s = Stored { head: "older".into(), reviewers: 1, shadow: false, blocked: false, skipped: false, why: None };
    let m = Mock::default().on(Method::Get, &format!("{GH}/issues/7/comments?per_page=100&page=1"), 200, comments(s.clone()));
    assert_eq!(reviewers(&m, &env, &t).unwrap().as_deref(), Some("Waiting for pr-guard to check the new commits"));
    assert_eq!(m.writes()[0].1, format!("{GH}/statuses/newer"));

    let shadow = Stored { head: "newer".into(), reviewers: 2, shadow: true, ..s };
    let m = Mock::default()
        .on(Method::Get, &format!("{GH}/issues/7/comments?per_page=100&page=1"), 200, comments(shadow))
        .on(Method::Get, &format!("{GH}/pulls/7/reviews?per_page=100&page=1"), 200, json!([]));
    assert_eq!(reviewers(&m, &env, &t).unwrap().as_deref(), Some("Shadow mode: would need 2 reviewers"));
    assert_eq!(m.writes()[0].2.as_ref().unwrap()["state"], "success");

    // No verdict yet: nothing to enforce, nothing written.
    let m = Mock::default().on(Method::Get, &format!("{GH}/issues/7/comments?per_page=100&page=1"), 200, json!([]));
    assert_eq!(reviewers(&m, &env, &t).unwrap(), None);
    assert!(m.writes().is_empty());

    // A blocking finding fails `pr-guard` in enforce mode and passes in shadow mode.
    let leak = || vec![Finding::at(Level::Danger, "secrets", "Looks like a token".into(), "src/a.ts", Some(1))];
    // A review event sets it again from the stored verdict, reason included.
    let status = |v: &Verdict| status_of(Client { http: &Mock::default(), t: &t }.api(), &Stored::of(v, "newer"), "newer", true).unwrap();
    let v = verdict(&enforce(), clean(), leak());
    assert_eq!(status(&v), (State::Failure, "Blocked: Looks like a token".into()));
    assert!(wanted_labels(&v).contains(&"pr-guard:blocked".to_string()));
    let v = verdict(&Policy { mode: Mode::Shadow, ..Policy::default() }, clean(), leak());
    assert_eq!(status(&v), (State::Success, "Shadow mode: would need 2 reviewers".into()));
}

#[test]
fn gitlab_uses_notes_label_lists_and_an_approval_rule() {
    let t = Target { auth: Auth::Private("glpat".into()), ..target(Kind::GitLab, "https://gitlab.com/api/v4", "group/sub/proj") };
    let mr = "https://gitlab.com/api/v4/projects/group%2Fsub%2Fproj/merge_requests/7";
    let m = Mock::default()
        .on(Method::Get, "https://gitlab.com/api/v4/user", 200, json!({"username": "project_9_bot"}))
        .on(Method::Get, &format!("{mr}/notes?sort=asc&order_by=created_at&per_page=100&page=1"), 200, json!([
            {"id": 10, "system": true, "author": {"username": "project_9_bot"}, "body": MARKER},
            {"id": 11, "system": false, "author": {"username": "project_9_bot"}, "body": format!("{MARKER}\nold")},
        ]))
        .on(Method::Get, mr, 200, json!({"labels": ["pr-guard:reviewers-0", "pr-guard:skip", "backend"], "sha": "abc123"}))
        .on(Method::Get, &format!("{mr}/approval_rules"), 200, json!([{"id": 5, "name": "Default", "approvals_required": 1}]));
    let policy = enforce();
    let v = verdict(&policy, risky(), vec![]);
    assert!(publish(&m, &Env::default(), &t, &policy, &v, &crate::report::comment(&v)).is_empty());
    let w = m.writes();
    assert_eq!((w[0].0, w[0].1.as_str()), (Method::Put, format!("{mr}/notes/11").as_str()));
    // The skip label is the people's: never removed.
    assert_eq!(w[1], (Method::Put, mr.to_string(), Some(json!({"add_labels": "pr-guard:reviewers-1", "remove_labels": "pr-guard:reviewers-0"}))));
    // The approval rule first; the status then passes and says GitLab holds the merge.
    assert_eq!(w[2], (Method::Post, format!("{mr}/approval_rules"), Some(json!({"name": "pr-guard", "approvals_required": 1}))));
    assert_eq!(w[3], (Method::Post, "https://gitlab.com/api/v4/projects/group%2Fsub%2Fproj/statuses/abc123".into(), Some(json!({
        "state": "success", "name": "pr-guard", "description": "Needs 1 reviewer, required by the merge request's approval rule", "target_url": "https://ci/run/1"
    }))));
    assert_eq!(w.len(), 4);
    assert!(m.log.borrow()[0].headers.contains(&("PRIVATE-TOKEN", "glpat".into())));

    // An existing rule is updated, and set to 0 rather than created when nobody is needed.
    let m = Mock::default().on(Method::Get, &format!("{mr}/approval_rules"), 200, json!([{"id": 6, "name": "pr-guard", "approvals_required": 2}]));
    let c = Client { http: &m, t: &t };
    assert!(gitlab::require_approvals(&c, 0).unwrap());
    assert_eq!(m.writes(), [(Method::Put, format!("{mr}/approval_rules/6"), Some(json!({"approvals_required": 0})))]);
}

#[test]
fn bitbucket_has_no_labels_and_fails_the_reviewers_build_until_approved() {
    let t = target(Kind::Bitbucket, "https://api.bitbucket.org/2.0", "team/app");
    let pr = "https://api.bitbucket.org/2.0/repositories/team/app/pullrequests/7";
    let m = Mock::default()
        .on(Method::Get, "https://api.bitbucket.org/2.0/user", 200, json!({"account_id": "557058:bot"}))
        .on(Method::Get, &format!("{pr}/comments?pagelen=100"), 200, json!({"values": [], "next": format!("{pr}/comments?pagelen=100&page=2")}))
        .on(Method::Get, &format!("{pr}/comments?pagelen=100&page=2"), 200, json!({"values": [
            {"id": 3, "deleted": true, "user": {"account_id": "557058:bot"}, "content": {"raw": MARKER}},
        ]}))
        .on(Method::Get, pr, 200, json!({"participants": [{"approved": true}, {"approved": false}]}));
    let policy = enforce();
    let mut v = verdict(&policy, risky(), vec![]);
    v.reviewers = 2;
    assert!(publish(&m, &Env::default(), &t, &policy, &v, &crate::report::comment(&v)).is_empty());
    let w = m.writes();
    assert_eq!((w[0].0, w[0].1.as_str()), (Method::Post, format!("{pr}/comments").as_str()));
    assert!(w[0].2.as_ref().unwrap()["content"]["raw"].as_str().unwrap().starts_with(MARKER));
    let status = "https://api.bitbucket.org/2.0/repositories/team/app/commit/abc123/statuses/build";
    // One build status, failed until the approvals (Bitbucket's merge check counts only builds that passed).
    assert_eq!(w[1], (Method::Post, status.into(), Some(json!({
        "key": "pr-guard", "state": "FAILED", "name": "pr-guard", "description": "Needs 2 reviewers: 1 of 2 approvals", "url": "https://ci/run/1"
    }))));
    assert_eq!(w.len(), 2);
}

#[test]
fn one_failing_part_does_not_stop_the_others() {
    let t = target(Kind::GitHub, "https://api.github.com", "acme/web");
    let m = Mock::default()
        .on(Method::Get, &format!("{GH}/issues/7/comments?per_page=100&page=1"), 200, json!([]))
        .on(Method::Get, &format!("{GH}/issues/7/labels?per_page=100&page=1"), 403, json!({"message": "Resource not accessible by integration"}))
        .on(Method::Get, &format!("{GH}/pulls/7/reviews?per_page=100&page=1"), 200, json!([{"user": {"login": "a"}, "state": "APPROVED"}]));
    let policy = enforce();
    let v = verdict(&policy, risky(), vec![]);
    let errors = publish(&m, &Env::from_pairs(&[("PR_GUARD_BOT", "bot")]), &t, &policy, &v, "x");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].starts_with("labels: GitHub Get") && errors[0].contains("HTTP 403"), "{}", errors[0]);
    let w = m.writes();
    assert_eq!(w.len(), 2);
    assert_eq!(w[1].2.as_ref().unwrap()["description"], "Passed, 1 of 1 approval");
}
