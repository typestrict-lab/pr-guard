# Install and run in CI

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

The model is downloaded on first use and cached; which one, and from where, is the policy's `model` (see
[policy and model](policy.md#choosing-the-model)).

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

## GitHub Action

`integrations/github/action.yml` installs a released binary, caches the model in `~/.cache/pr-guard`, runs
`pr-guard check` with the workflow's token (comment, labels and the status, see [publishing to the host](hosts.md)) and uploads the
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
[`integrations/github/examples/pr-guard-reviews.yml`](../integrations/github/examples/pr-guard-reviews.yml): it runs the
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

A [CI/CD component](../integrations/gitlab/templates/pr-guard.yml) that runs on merge request pipelines in the Docker
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

A [pipe](../integrations/bitbucket/) on top of the Docker image; the JUnit report lands in `test-results/`, which Pipelines
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

An [orb](../integrations/circleci/orb.yml) with an `install` command, a `check` command and a `check` job (`cimg/base`,
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

A [step](../integrations/bitrise/step.yml) that installs the binary and writes the JUnit report to the Tests tab
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

No plugin: the [Jenkinsfile example](../integrations/jenkins/Jenkinsfile) runs the Docker image on pull request builds of a
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
