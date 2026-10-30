#!/bin/sh
# Reports commits on upstream's default branch that this fork has not merged.
#
# The release watcher (upstream-watch.yaml, job `check`) only reacts to a new GitHub
# release. Upstream also lands fixes on its default branch long before a release:
# the bounded punch-request store and the key-file permissions (October 2026) sat
# there unreleased and were only found by asking. This keeps ONE issue open while
# such commits exist, rewrites it when more arrive, and closes it once the fork's
# recorded base (UPSTREAM_COMMIT in UPSTREAM_VERSION) has caught up. It never merges:
# a human does that.
#
# Environment:
#   GH_TOKEN       token for `gh` (issues: write on this repository)
#   UPSTREAM_REPO  default rustdesk/rustdesk-server
#   THIS_REPO      default $GITHUB_REPOSITORY
#   PRETEND_BASE   compare from this commit instead of UPSTREAM_COMMIT (testing)
#   DRY_RUN=1      print what would be done, change nothing
set -eu
cd "$(git rev-parse --show-toplevel)"
. ./UPSTREAM_VERSION
REPO="${UPSTREAM_REPO:-rustdesk/rustdesk-server}"
THIS="${THIS_REPO:-${GITHUB_REPOSITORY:?set THIS_REPO}}"
BASE="${PRETEND_BASE:-$UPSTREAM_COMMIT}"
BRANCH=$(gh api "repos/$REPO" --jq .default_branch)
HEAD=$(gh api "repos/$REPO/commits/$BRANCH" --jq .sha)
TITLE="Upstream $BRANCH has commits not merged here"

act() {  # run, or only describe in a dry run
    if [ "${DRY_RUN:-}" = 1 ]; then echo "[dry run] $*"; else "$@"; fi
}

AHEAD=$(gh api "repos/$REPO/compare/$BASE...$HEAD" --jq .ahead_by)
EXISTING=$(gh issue list --repo "$THIS" --state open --search "\"$TITLE\" in:title" \
    --json number,title --jq "map(select(.title == \"$TITLE\")) | .[0].number // empty")

echo "upstream $REPO $BRANCH = $HEAD"
echo "fork base               = $BASE"
echo "commits not merged      = $AHEAD"

if [ "$AHEAD" -eq 0 ]; then
    if [ -n "$EXISTING" ]; then
        act gh issue close "$EXISTING" --repo "$THIS" \
            --comment "Merged: the fork's base is now \`$BASE\`, which includes upstream \`$BRANCH\` at \`$HEAD\`."
    fi
    echo "Nothing to report."
    exit 0
fi

BODY=$(mktemp); trap 'rm -f "$BODY"' EXIT
{
    echo "Upstream \`$REPO\` \`$BRANCH\` has **$AHEAD** commit(s) that this fork has not merged."
    echo "Nothing is merged automatically; read them, then merge by hand."
    echo
    gh api "repos/$REPO/compare/$BASE...$HEAD" --jq \
        '.commits[] | "- [`\(.sha[0:7])`](\(.html_url)) \(.commit.committer.date[0:10]) \(.commit.message | split("\n")[0]) (\(.commit.author.name))"'
    echo
    echo "All changes: https://github.com/$REPO/compare/$BASE...$HEAD"
    echo
    echo "To merge (see NOTES.md, \"Upstream merge 2026-10-29\", for how the last one went):"
    echo
    echo '```'
    echo "git fetch upstream $BRANCH"
    echo "git merge --no-ff upstream/$BRANCH"
    echo "# resolve, test, then set UPSTREAM_COMMIT=$HEAD in UPSTREAM_VERSION"
    echo '```'
    echo
    echo "This issue is rewritten when upstream moves and closed when UPSTREAM_COMMIT catches up."
    echo
    echo "<!-- upstream-head: $HEAD -->"
} > "$BODY"

if [ -z "$EXISTING" ]; then
    act gh issue create --repo "$THIS" --title "$TITLE" --body-file "$BODY"
else
    SEEN=$(gh issue view "$EXISTING" --repo "$THIS" --json body --jq .body \
        | sed -n 's/.*<!-- upstream-head: \([0-9a-f]*\) -->.*/\1/p')
    if [ "$SEEN" = "$HEAD" ]; then
        echo "Issue #$EXISTING already describes $HEAD."
    else
        act gh issue edit "$EXISTING" --repo "$THIS" --body-file "$BODY"
        act gh issue comment "$EXISTING" --repo "$THIS" \
            --body "Upstream moved to \`$HEAD\`; the list above is updated."
    fi
fi
if [ "${DRY_RUN:-}" = 1 ]; then echo "--- body ---"; cat "$BODY"; fi
