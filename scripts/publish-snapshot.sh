#!/usr/bin/env bash
# Export the public source tree of Spotless into a clean directory, as one commit, for pushing to
# the public repo (github.com/fjenn/spotless). Our own history never leaves this machine.
#
#   scripts/publish-snapshot.sh [--ref REF] [--out DIR] [--message MSG] [--scan-only DIR]
#
#   --ref REF        what to export (default: main). Only tracked files of REF are exported, so
#                    untracked and gitignored files (CLAUDE.md, .claude/, worktrees, build
#                    outputs, local keys) can never get in.
#   --out DIR        the snapshot (default: ~/music-app/public-snapshot).
#                      - no DIR/.git yet → a new repo whose only commit is this tree;
#                      - DIR/.git exists (e.g. a clone of fjenn/spotless) → its files are replaced
#                        by this tree and that is committed on top, so later releases push without
#                        --force. The previous public commits stay; our private history still
#                        never appears.
#   --message MSG    commit message (default: "Spotless <version>").
#   --scan-only DIR  only run the secret / personal-data scan over DIR and exit.
#
# Commit identity: SNAPSHOT_AUTHOR_NAME / SNAPSHOT_AUTHOR_EMAIL (default: fjenn and the GitHub
# no-reply address, so no personal mail address ends up in the public history).
#
# What is left out (EXCLUDE below), and why:
#   - Spotify reference material and comparison screenshots: docs/audit, docs/spotify-spec,
#     docs/compare, docs/research, and docs/bench (benchmarks scored against scraped Spotify
#     output, Spotify web-player scrapes, absolute local paths, .pyc files).
#   - Internal work notes: docs/streams (per-stream agent reports, worktree paths, owner
#     feedback), docs/BUILD-STATUS.md.
#   - Upstream Limusic leftovers that do not apply to Spotless: website/ (limusic.app, with its
#     Google site-verification file), assets/docs (Limusic banner images), .github/FUNDING.yml
#     (the upstream author's sponsor links) and Limusic's release/CI workflows (Linux/Windows
#     releases, its website, nightly stream health), which would run against the public repo with
#     secrets it does not have. Our own workflows are kept (everything else in .github/workflows/):
#     ci.yml (checks), release-mac.yml (signed, notarized Mac releases from the repo's secrets)
#     and release-ios.yml (TestFlight uploads; it needs ios/Encore/ExportOptions.plist), see
#     docs/RELEASING.md; a guard below stops the export if one of them is missing. There is no GitHub
#     Pages site: the website is opena.tech/spotless, hosted elsewhere.
#   - crates/core/tests/fixtures/lyrics: the lyrics benchmark's captured provider answers, which
#     are the full lyrics of 100+ songs (the test that reads them skips when they are absent).
#   - Junk: __pycache__, *.pyc, .DS_Store.
# Rewritten (TRANSFORM below): the Apple team ID in ios/Encore (project.yml + project.pbxproj) is
# blanked; builders set their own in Xcode, and release-ios.yml writes it back at build time (from
# the APPLE_TEAM_ID repo variable, or App Store Connect). ios/Encore/ExportOptions.plist is committed
# with an empty teamID for the same reason. It is not secret, but it is not needed either.
#
# Then the scan (scan() below) runs over the result and the script stops on any hit that is not on
# its allow-list, before anything is committed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

REF=main
OUT="${HOME}/music-app/public-snapshot"
MSG=""
SCAN_ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --ref) REF="$2"; shift ;;
    --out) OUT="$2"; shift ;;
    --message) MSG="$2"; shift ;;
    --scan-only) SCAN_ONLY="$2"; shift ;;
    -h|--help) sed -n '2,46p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
die() { echo "ERROR: $*" >&2; exit 1; }

EXCLUDE=(
  docs/audit
  docs/spotify-spec
  docs/compare
  docs/research
  docs/bench
  docs/streams
  docs/BUILD-STATUS.md
  crates/core/tests/fixtures/lyrics
  website
  assets/docs
  .github/FUNDING.yml
  .github/workflows/checks.yml
  .github/workflows/karaoke-gpu-linux.yml
  .github/workflows/linux-release.yml
  .github/workflows/macos-release.yml
  .github/workflows/stream-health.yml
  .github/workflows/website.yml
  .github/workflows/windows-release.yml
)
# Anything Spotify-reference-like that might appear later under another name.
EXCLUDE_NAMES=('*spotify-spec*' '*spotify_scrape*' '__pycache__' '*.pyc' '.DS_Store')

# ---------------------------------------------------------------------------------------------
# The scan. Prints every hit as "kind  file:line: text"; returns 1 when a hit is not allowed.
#   hard  → stops the export: personal names / paths, keys, tokens, certificate hashes, key files
#   info  → printed for review, does not stop (third-party addresses in fixtures, the public
#           GitHub handle)
# ---------------------------------------------------------------------------------------------
scan() {
  local dir="$1" bad=0 hits
  [ -d "$dir" ] || die "scan: no directory $dir"
  echo "==> scanning $dir"
  # Files that must never be published, by name.
  hits="$(cd "$dir" && find . -path ./.git -prune -o -type f \( -name '*.p8' -o -name '*.p12' \
    -o -name '*.pem' -o -name '*.key' -o -name '*.mobileprovision' -o -name '*.cer' \
    -o -name '.env' -o -name '.env.*' -o -name 'lastfm.keys' -o -name '*.keystore' \
    -o -name '.DS_Store' -o -name '*.pyc' -o -name 'CLAUDE.md' \) -print)"
  if [ -n "$hits" ]; then echo "$hits" | sed 's/^/hard  file: /'; bad=1; fi
  hits="$(cd "$dir" && find . -path ./.git -prune -o -type d \( -name .claude -o -name worktrees \
    -o -name node_modules -o -name target -o -name __pycache__ \) -print)"
  if [ -n "$hits" ]; then echo "$hits" | sed 's/^/hard  dir: /'; bad=1; fi

  # This script is skipped by the content checks: it has to spell the patterns out.
  local g=(grep -r -n -I --exclude-dir=.git --exclude='pnpm-lock.yaml' --exclude='Cargo.lock'
           --exclude='publish-snapshot.sh')
  # Personal paths. /Users/x, /Users/me, /Users/you are placeholders in tests.
  hits="$(cd "$dir" && "${g[@]}" -E '/Users/' . | grep -v -E '/Users/(x|me|you)([/"]|$)' || true)"
  [ -z "$hits" ] || { echo "$hits" | sed 's/^/hard  path: /'; bad=1; }
  # Personal names, worked out at run time so none is written in this (public) file: this Mac's
  # login and full name, git's user.name/email, and one extended regex per line from
  # $SNAPSHOT_PRIVATE_PATTERNS (default ~/.config/spotless/snapshot-private-patterns, outside
  # the repo; put company names, other handles, old usernames there). In test fixtures such words
  # turn up in real song lyrics and artist names, so hits there are shown, not fatal.
  local names=() w
  for w in $(id -un) $(id -F 2>/dev/null) $(git config user.name 2>/dev/null) \
           $(git config user.email 2>/dev/null | sed 's/@.*//; s/[._+-]/ /g'); do
    [ "${#w}" -ge 4 ] && names+=("$w")
  done
  local pf="${SNAPSHOT_PRIVATE_PATTERNS:-$HOME/.config/spotless/snapshot-private-patterns}"
  if [ -f "$pf" ]; then
    while IFS= read -r w; do [ -n "$w" ] && [ "${w#\#}" = "$w" ] && names+=("$w"); done < "$pf"
  fi
  if [ ${#names[@]} -gt 0 ]; then
    local re; re="$(printf '%s|' "${names[@]}")"; re="\\b(${re%|})\\b"
    # The public GitHub handle is expected (repo URLs); report it below instead.
    hits="$(cd "$dir" && "${g[@]}" -i -E "$re" . | grep -v -i -E 'github\.com/fjenn|fjenn\.github\.io|fjenn/spotless|fjenn@users\.noreply' | cut -c1-200 || true)"
    if [ -n "$hits" ]; then
      echo "$hits" | grep -E '/fixtures/' | sed 's/^/info  name (fixture): /' || true
      if echo "$hits" | grep -q -v -E '/fixtures/'; then
        echo "$hits" | grep -v -E '/fixtures/' | sed 's/^/hard  name: /'; bad=1
      fi
    fi
  fi
  # Secrets: cloud / GitHub / OpenAI-style keys, private key blocks, Developer ID identities with a
  # real name, certificate SHA-1 hashes (40 upper-case hex), an Apple team ID left in the project.
  hits="$(cd "$dir" && "${g[@]}" -E 'AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|gh[pousr]_[0-9A-Za-z]{36}|github_pat_[0-9A-Za-z_]{20,}|\bsk-[A-Za-z0-9]{20,}|xox[abpr]-[0-9A-Za-z-]{10,}|-----BEGIN [A-Z ]*PRIVATE KEY-----|untrusted comment: minisign secret|rsign encrypted secret key|Developer ID Application: [^"<N$]|\b[0-9A-F]{40}\b|DEVELOPMENT_TEAM[:=] *"?[A-Z0-9]{10}|DevelopmentTeam = [A-Z0-9]{10}|--team-id [A-Z0-9]{10}' . || true)"
  [ -z "$hits" ] || { echo "$hits" | sed 's/^/hard  secret: /'; bad=1; }
  # Generic "key = long literal" assignments.
  hits="$(cd "$dir" && "${g[@]}" -i -E '(api[_-]?key|api[_-]?secret|client[_-]?secret|access[_-]?token|password)["'"'"']?[[:space:]]*[:=][[:space:]]*["'"'"'][A-Za-z0-9_/+=-]{20,}["'"'"']' . || true)"
  [ -z "$hits" ] || { echo "$hits" | sed 's/^/hard  literal: /'; bad=1; }

  # For review only.
  hits="$(cd "$dir" && "${g[@]}" -o -E '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[a-z]{2,}' . \
    | grep -v -E '@[0-9]x\.|@(example|example\.invalid|evil\.example)|noreply|@[0-9]+\.png' | sort -u || true)"
  [ -z "$hits" ] || echo "$hits" | sed 's/^/info  email: /'
  hits="$(cd "$dir" && "${g[@]}" -i -E 'fjenn' . | grep -v -E 'fjenn/spotless|fjenn\.github\.io/spotless' | cut -c1-200 || true)"
  [ -z "$hits" ] || echo "$hits" | sed 's/^/info  handle (other than the repo URL): /'

  if [ "$bad" = 0 ]; then echo "scan: clean (no hard hits)"; else echo "scan: HARD HITS above"; fi
  return "$bad"
}

if [ -n "$SCAN_ONLY" ]; then scan "$SCAN_ONLY"; exit $?; fi

git rev-parse --verify --quiet "$REF^{commit}" >/dev/null || die "no commit $REF"
SHA="$(git rev-parse --short "$REF")"
VERSION="$(git show "$REF:src-tauri/tauri.conf.json" | sed -n 's/^  "version": "\(.*\)",/\1/p')"
MSG="${MSG:-Spotless $VERSION}"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
echo "==> exporting $REF ($SHA) → $STAGE"
git archive --format=tar "$REF" | tar -x -C "$STAGE"

echo "==> removing what stays private"
for p in "${EXCLUDE[@]}"; do
  if [ -e "$STAGE/$p" ]; then rm -rf "${STAGE:?}/$p"; echo "  - $p"; fi
done
for n in "${EXCLUDE_NAMES[@]}"; do
  (cd "$STAGE" && find . -name "$n" -prune -print) | while read -r f; do
    rm -rf "${STAGE:?}/${f#./}"; echo "  - ${f#./}"
  done
done

# The public repo builds its releases itself; never publish it without the workflows that do that.
for wf in .github/workflows/release-mac.yml .github/workflows/release-windows.yml .github/workflows/release-ios.yml .github/workflows/ci.yml \
          ios/Encore/ExportOptions.plist scripts/ci/asc-team-id.rb; do
  [ -f "$STAGE/$wf" ] || die "$wf is missing from the export (deleted, or caught by EXCLUDE?)"
done

echo "==> transforms"
for f in ios/Encore/project.yml ios/Encore/Encore.xcodeproj/project.pbxproj; do
  [ -f "$STAGE/$f" ] || continue
  sed -i '' -E 's/(DEVELOPMENT_TEAM: )"[A-Z0-9]{10}"/\1""/; s/(DEVELOPMENT_TEAM|DevelopmentTeam) = [A-Z0-9]{10};/\1 = "";/' "$STAGE/$f"
  echo "  ~ $f: Apple team ID blanked"
done

scan "$STAGE" || die "the scan found something that must not be published; fix it in the repo (or extend EXCLUDE) and re-run"

echo "==> writing $OUT"
mkdir -p "$OUT"
if [ ! -d "$OUT/.git" ]; then
  [ -z "$(ls -A "$OUT")" ] || die "$OUT exists, is not empty and is not a git repo; move it away first"
  git -C "$OUT" init -q -b main
fi
# Replace the tracked content wholesale (keeps .git, so a clone of the public repo stays a clone).
find "$OUT" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
ditto "$STAGE" "$OUT"

NAME="${SNAPSHOT_AUTHOR_NAME:-fjenn}"
EMAIL="${SNAPSHOT_AUTHOR_EMAIL:-}"
if [ -z "$EMAIL" ]; then
  ID="$(gh api user --jq .id 2>/dev/null || true)"
  EMAIL="${ID:+$ID+}fjenn@users.noreply.github.com"
fi
git -C "$OUT" add -A
if git -C "$OUT" diff --cached --quiet; then
  echo "nothing changed since the last snapshot commit"
else
  # UTC, so the public commit does not carry this Mac's time zone.
  TZ=UTC GIT_AUTHOR_NAME="$NAME" GIT_AUTHOR_EMAIL="$EMAIL" GIT_COMMITTER_NAME="$NAME" GIT_COMMITTER_EMAIL="$EMAIL" \
    git -C "$OUT" -c commit.gpgsign=false commit -q -m "$MSG"
fi
echo
echo "snapshot: $OUT  ($(git -C "$OUT" rev-list --count HEAD) commit(s), $(git -C "$OUT" ls-files | wc -l | tr -d ' ') files, from $REF $SHA)"
echo "author:   $NAME <$EMAIL>"
echo "publish (not done by this script):"
echo "  gh repo create fjenn/spotless --public --source \"$OUT\" --push    # first time"
echo "  git -C \"$OUT\" push origin main                                    # afterwards"
