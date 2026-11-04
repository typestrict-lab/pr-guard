# pr-guard

A pull request guard that runs where your code already is. It reads the diff and the PR description, asks
TS-PRGuard (a 150M-parameter model, 8-bit ONNX on the CPU) seven questions about the change, adds deterministic checks
(secrets, sensitive paths, destructive SQL, size) and lets a YAML policy decide how many reviewers the change needs,
whether it blocks, and whether it can merge on its own.

No API, no key, no telemetry: the code never leaves the machine.

## Status

Early. The CLI reads the change from git, detects the CI it runs on, writes the formats most CIs show (SARIF,
GitLab Code Quality, JUnit) and, with a token, publishes the verdict on GitHub, GitLab or Bitbucket (sticky comment,
labels, statuses, required reviewers). No release has been published yet. The model is the first multi-task TS-PRGuard; the default
policy runs in shadow mode (report, never fail): set `mode: enforce` once the guard's answers on your own pull requests
look right.

## The questions

| Question | Used as |
|---|---|
| Does the change do what the description says? | weighs in the score |
| Does it introduce a security vulnerability? | warning |
| Does it break compatibility for users? | warning |
| Do the tests cover the changed code? | warning |
| How hard is it to review? (5 bands) | weighs in the score |
| What kind of change is it? | label only |
| Does it break the public API? | reported, not used yet |

The questions and their options come from the model package when its `decision_ai.json` lists them (`tasks`);
pr-guard only asks the tasks the package lists.

### Rules before the model

What the changed files alone settle is answered by a rule, before the model and also with `--no-model`; the model is
not asked those questions.

| Answer | Rule |
|---|---|
| Type `Documentation only` | every file is documentation (`.md`, `.rst`, `.adoc`, `.txt`, `docs/` or `doc/`, `LICENSE`, `CHANGELOG`, `README`) |
| Type `Tests only` | every file is a test |
| Type `Build system, dependencies or continuous integration` | every file is a manifest, lock file, build script, Dockerfile or CI configuration |
| Type `Formatting or style only` | every hunk only moves whitespace (removed and added text equal once whitespace is dropped, and outside Python a trailing comma before `)`, `]` or `}`); formatter settings may come along |
| Tests not applicable | no production code changed beyond whitespace (docs, tests, configuration, build, formatting) |

The rules run in that order. Any other change (code, or a mix) goes to the model: whether the tests cover the changed
code, and the type among bug fix, new feature, refactoring, performance improvement and maintenance when the package
lists the type task. A package without it leaves the type empty for code changes; the type is a label only and never
weighs in the score. The text report
marks a rule answer with `(by rule)`, the JSON verdict with `change_type_source` and `tests_source` (`rule` or `model`)
next to `tests_applicable`, and the log on stderr with `pr-guard: prg-tipo rule <type>`.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/typestrict-lab/pr-guard/main/install.sh | sh
curl -fsSL https://raw.githubusercontent.com/typestrict-lab/pr-guard/main/install.sh | sh -s -- 0.1.0 --dir /usr/local/bin
```

The script picks the build for the machine, checks the archive against the SHA-256 published next to it and installs
into `~/.local/bin` (or `--dir`; `PR_GUARD_VERSION` and `PR_GUARD_INSTALL_DIR` work too). Other ways:

| Where | How |
|---|---|
| Release archives | `pr-guard-<target>.tar.gz` (Windows also `.zip`) on the [releases](https://github.com/typestrict-lab/pr-guard/releases), with `SHA256SUMS` |
| Homebrew | `brew install typestrict-lab/tap/pr-guard` once the formula attached to each release (`pr-guard.rb`) is in the tap |
| Docker | `ghcr.io/typestrict-lab/pr-guard:0.1.0`, linux/amd64 and linux/arm64, with the model inside |
| Cargo | `cargo install --git https://github.com/typestrict-lab/pr-guard --tag v0.1.0` |

Builds: Linux x64 and arm64 (built on Ubuntu 22.04, so glibc 2.35 or newer; no musl/Alpine build, use the Docker image
there), macOS arm64 and x64, Windows x64. Each archive is self-contained: unpack it anywhere and run.

The image runs in the repository mounted at `/repo`:

```sh
docker run --rm -v "$PWD:/repo" ghcr.io/typestrict-lab/pr-guard:0.1.0 check --base origin/main
```

## Usage

```sh
pr-guard init > .github/pr-guard.yml        # the default policy, to adjust
pr-guard check                              # HEAD against its merge base with the default branch
pr-guard check --base origin/release --head my-branch
pr-guard check --staged --no-model          # the staged changes, deterministic checks only (pre-commit)
git diff main... | pr-guard check --diff -  # any diff, from stdin or a file
pr-guard check --format sarif --output pr-guard.sarif --format markdown --output pr-guard.md
pr-guard check --pr 42                      # publish to pull request 42 (needs a host token)
pr-guard reviewers                          # on a review event: re-count approvals against the stored verdict
```

Exit code: 0 passed, 1 blocked by the policy, 2 usage or runtime error. Shadow mode always exits 0; `--mode shadow` or
`--mode enforce` overrides the policy's mode.

The model package (`decision_ai.json`, `tokenizer.json`, `onnx/model_8bit.onnx`, 168 MB) is read from `--model`,
`$PR_GUARD_MODEL`, or `~/.cache/pr-guard/ts-prguard` (`$PR_GUARD_CACHE` to move the cache). The cache is filled from
[typestrict/TS-PRGuard](https://huggingface.co/typestrict/TS-PRGuard) on first use, at the revision this release was
tested with, and every file is checked against its SHA-256 before it is used. `pr-guard model pull` downloads it ahead
of time (Docker images, runners without internet at check time); `$PR_GUARD_HF_ENDPOINT` points at a mirror.

### Where the change comes from

Without `--diff`, pr-guard asks the local `git`. The range ends at `--head` (default `HEAD`) and starts at the merge base
with `--base`; without `--base` it uses the branch the pull request targets when the CI says so, else `origin/HEAD`,
`main` or `master`. CI checkouts are often shallow: fetch the history (`fetch-depth: 0` with `actions/checkout`) so the
merge base exists.

The description is `--description` or `--description-file` when given; else the pull request's title and body when the
CI has them (GitHub's event payload, GitLab's merge request variables, Jenkins' `CHANGE_TITLE`); else the commit
messages of the range, newest first.

### CI detection

pr-guard reads the environment of GitHub Actions, GitLab CI, Bitbucket Pipelines, Jenkins, CircleCI, Bitrise, Buildkite
and Azure Pipelines to find the base branch, the head commit, the pull request number and the repository. Detection
makes no network call. With no CI it runs as on a laptop. On GitHub Actions it also prints workflow commands, so each
finding shows as an annotation on its file and line.

### Output formats

`--format` can repeat, each with its own `--output` in the same order (`-` is stdout). A single `--format` without
`--output` goes to stdout; with no `--format` the text summary goes to stdout.

| Format | What it is | Read by |
|---|---|---|
| `text` | the summary, colored on a terminal (`NO_COLOR` turns colors off) | people |
| `json` | the verdict | scripts |
| `markdown` | the sticky comment (`--comment <path>` is a shorthand) | pull request comments, job summaries |
| `sarif` | SARIF 2.1.0, one rule per check | GitHub code scanning, Azure DevOps |
| `codeclimate` | Code Climate issues | GitLab Code Quality in the merge request |
| `junit` | one test case for the verdict, each question and each finding | Jenkins, CircleCI, Bitrise, most CIs |

In JUnit a case fails at danger level and the verdict case fails when the policy blocks; nothing fails in shadow mode.

## Publishing to the host

Where the pull request lives is a separate question from where the job runs: a Jenkins job can review a GitHub pull
request. pr-guard reads the host from the `origin` remote and, when it has a token and a pull request number, writes:

| What | GitHub | GitLab | Bitbucket Cloud |
|---|---|---|---|
| Sticky comment (created once, then edited) | issue comment | merge request note | pull request comment |
| Labels `pr-guard:reviewers-N`, `pr-guard:blocked`, `pr-guard:type-…`, `pr-guard:not-evaluated`, `pr-guard:auto-merge` | yes | yes | no labels on Bitbucket |
| Status `pr-guard`: failed when the policy blocks, waiting until the approvals the verdict asks for, else passed (always passed in shadow mode) | commit status, pending while approvals are missing | commit status; the approvals go to the merge request's `pr-guard` approval rule (`approvals_required`, Premium) | build status, failed while approvals are missing |

| Host | Found by | Token |
|---|---|---|
| GitHub | `github.com`, or the runner's `GITHUB_SERVER_URL` (Enterprise Server, API from `GITHUB_API_URL`) | `GITHUB_TOKEN` (or `GH_TOKEN`) |
| GitLab | `gitlab.com`, or the job's `CI_SERVER_URL` (self-managed, API from `CI_API_V4_URL`) | `GITLAB_TOKEN`: a project access token with the `api` scope; `CI_JOB_TOKEN` cannot write notes, labels or statuses |
| Bitbucket | `bitbucket.org` | `BITBUCKET_TOKEN`: a repository access token (Bearer), or an API token with `BITBUCKET_USERNAME` (Basic) |

`PR_GUARD_HOST` (`github`, `gitlab`, `bitbucket`) and `PR_GUARD_API_URL` name a self-hosted server the CI does not
describe (Jenkins against GitHub Enterprise, say). The pull request number comes from the CI, or `--pr`. Without a token
or a number pr-guard only prints, as on a laptop; `--no-publish` prints only even with both. A failed API call is a
warning on stderr and never changes the exit code. The policy can turn each part off:

```yaml
publish:
  comment: true
  labels: true      # the skip label is never removed; only pr-guard:* labels move
  status: true
  reviewers: true
```

The comment ends with the verdict as JSON inside an HTML comment (head commit, reviewers, shadow, blocked). `pr-guard
reviewers` reads it back on a review event, counts the approvals and sets `pr-guard` again without the diff or
the model; a verdict for an older commit leaves the status waiting for the next check. Only comments written by the
token's own account are read or edited, so a pasted copy of the marker cannot lower the number of reviewers. When the
token cannot say who it is (GitHub's `GITHUB_TOKEN` outside Actions, some Bitbucket tokens), set `PR_GUARD_BOT` to the
account's login (GitHub, GitLab) or `account_id` (Bitbucket); on GitHub Actions it is `github-actions[bot]`.

To make the verdict binding, add `pr-guard` to the required status checks (GitHub branch
protection or ruleset) or enable the "passing builds" merge check (Bitbucket). On GitLab the approval rule is enforced
by GitLab itself; on GitLab Free, where merge request approval rules do not exist, the call fails with a warning.

## GitHub Action

`integrations/github/action.yml` installs a released binary, caches the model in `~/.cache/pr-guard`, runs
`pr-guard check` with the workflow's token (comment, labels, statuses and required reviewers, as above) and uploads the
SARIF to code scanning. The sticky comment's markdown also goes to `pr-guard.md` and to the job summary.

```yaml
on: pull_request
permissions:
  contents: read
  pull-requests: write   # comment and labels
  statuses: write        # the pr-guard status
  security-events: write # SARIF
jobs:
  pr-guard:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: typestrict-lab/pr-guard/integrations/github@main
        with:
          mode: shadow        # or enforce; empty keeps the policy's mode
          # policy: .github/pr-guard.yml
          # args: --label something
          # version: 0.1.0
          # token: ''         # print only
```

The required reviewers are re-counted on each review by a second, small workflow,
[`integrations/github/examples/pr-guard-reviews.yml`](integrations/github/examples/pr-guard-reviews.yml): it runs the
action with `command: reviewers` on `pull_request_review`, needs no checkout, and finishes in seconds. On pull requests
from forks GitHub hands both workflows a read-only token: the verdict is then printed and in the job summary, but not
published.

The action never runs code from the pull request: it only reads the diff that the checkout already brought.

## pre-commit

With the `pr-guard` binary on `PATH`, add to `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/typestrict-lab/pr-guard
    rev: v0.1.0
    hooks:
      - id: pr-guard
```

The hook runs `pr-guard check --staged --no-model`: secrets, sensitive paths, destructive SQL and size on the staged
changes, without the model, in well under a second.

Without the binary installed, `id: pr-guard-docker` runs the same check in the Docker image.

## Other CIs

Each integration is a thin shell: it installs pr-guard (or uses the image), runs `pr-guard check` and hands the findings
to the CI's own report. None checks out or runs code from the pull request; pr-guard only reads the diff. All need the
history for the merge base (no shallow clone) and fetch the target branch themselves. `mode` left empty keeps the
policy's mode, shadow by default; `enforce` fails the job when the policy blocks. The sources are under
`integrations/`.

### GitLab CI

A [CI/CD component](integrations/gitlab/templates/pr-guard.yml) that runs on merge request pipelines in the Docker
image. Findings go to the merge request through Code Quality and the test report.

```yaml
include:
  - component: $CI_SERVER_FQDN/typestrict-lab/pr-guard/pr-guard@0.1.0
    inputs:
      mode: shadow
```

The catalog needs the component in a GitLab project (`templates/` at its root, mirrored from `integrations/gitlab/`).
Until then, or on a self-managed GitLab, include the file with `include: remote:` and the same `inputs`.

### Bitbucket Pipelines

A [pipe](integrations/bitbucket/) on top of the Docker image; the JUnit report lands in `test-results/`, which Pipelines
shows on the pull request.

```yaml
pipelines:
  pull-requests:
    '**':
      - step:
          name: pr-guard
          clone:
            depth: full
          script:
            - pipe: docker://ghcr.io/typestrict-lab/pr-guard-bitbucket-pipe:0.1.0
              variables:
                MODE: shadow
                # POLICY: .github/pr-guard.yml
                # ARGS: --no-model
```

### CircleCI

An [orb](integrations/circleci/orb.yml) with an `install` command, a `check` command and a `check` job (`cimg/base`,
model cached per version, JUnit to the test results). CircleCI does not say which branch a pull request targets, so
pass it as `base`.

```yaml
version: 2.1
orbs:
  pr-guard: typestrict-lab/pr-guard@0.1.0
workflows:
  review:
    jobs:
      - pr-guard/check:
          base: main
          mode: shadow
```

### Bitrise

A [step](integrations/bitrise/step.yml) that installs the binary and writes the JUnit report to the Tests tab
(`BITRISE_TEST_DEPLOY_DIR`); outputs `PR_GUARD_EXIT_CODE` and `PR_GUARD_COMMENT_PATH`. Run it after Git Clone with full
history.

```yaml
- git::https://github.com/typestrict-lab/pr-guard.git@v0.1.0:
    inputs:
      - mode: shadow
```

Bitrise reads `step.yml` from the root of the repository it points at, so this works once the step is in the StepLib
or in a repository of its own; the files in `integrations/bitrise/` are that repository.

### Jenkins

No plugin: the [Jenkinsfile example](integrations/jenkins/Jenkinsfile) runs the Docker image on pull request builds of a
multibranch pipeline (`CHANGE_TARGET`, `CHANGE_TITLE`) and publishes the JUnit report.

```groovy
stage('pr-guard') {
  when { changeRequest() }
  agent { docker { image 'ghcr.io/typestrict-lab/pr-guard:0.1.0'; args '--entrypoint=""'; reuseNode true } }
  steps {
    sh 'git fetch --quiet origin "+refs/heads/${CHANGE_TARGET}:refs/remotes/origin/${CHANGE_TARGET}"'
    sh 'pr-guard check --format junit --output pr-guard.xml --format text --output -'
  }
  post { always { junit allowEmptyResults: true, testResults: 'pr-guard.xml' } }
}
```

## Examples

`examples/` has small projects in several languages with pull requests that plant real problems (SQL injection, a
removed permission check, a renamed public function, a description that says something else) next to clean ones.

```sh
bash examples/run.sh
```

## Development

```sh
cargo test                                         # parity tests skip without the weights
PR_GUARD_MODEL=/path/to/model cargo test --release
```

### Releases

A `v*` tag (matching `version` in `Cargo.toml`) runs `.github/workflows/release.yml`: it builds every target, runs each
archive against the real model, attaches the archives, their `.sha256`, `SHA256SUMS` and the Homebrew formula to the
GitHub release, and pushes the multi-arch image to `ghcr.io/typestrict-lab/pr-guard` (plus the Bitbucket Pipe image).
`scripts/fetch-model.sh` pins the model revision and hashes for the image and the smoke test; bump it with `src/pull.rs`.

### How ONNX Runtime is linked

`ort` with `download-binaries` links pyke's prebuilt ONNX Runtime (1.28) **statically** where pyke ships one; elsewhere
the build links Microsoft's shared library and the archive carries it next to the binary.

| Target | ONNX Runtime | Ships next to the binary | Other runtime dependencies |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | static (pyke) | nothing | glibc, libstdc++, libgcc_s |
| `aarch64-unknown-linux-gnu` | static (pyke) | nothing | glibc, libstdc++, libgcc_s |
| `aarch64-apple-darwin` | static (pyke, CoreML build) | nothing | system libc++, Foundation, CoreML |
| `x86_64-apple-darwin` | shared: Microsoft's 1.23.2, their last macOS x64 build (`ORT_LIB_PATH`, `ORT_PREFER_DYNAMIC_LINK`, rpath `@executable_path`); cross-compiled on Apple silicon | `libonnxruntime.1.23.2.dylib` | system libc++ |
| `x86_64-pc-windows-msvc` | static (pyke, DirectML build) | `DirectML.dll` (imported at load; Windows' own copy is older) | the Visual C++ runtime (`vcruntime140.dll`), as any Rust MSVC binary |

pr-guard asks for ONNX Runtime's C API version 17, so the 1.23.2 library on macOS x64 is enough and gives the same
answers as the static 1.28 build (same probabilities on a smoke diff, run under Rosetta). There is no musl build: pyke ships none and
Microsoft's Linux builds link glibc.

## License

Apache-2.0
