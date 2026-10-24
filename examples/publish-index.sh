#!/bin/sh
# Publishes a Herdr plugin catalog from a plain git server — no web UI or
# API needed. Run it on the server from cron (or from CI):
#
#   */30 * * * * /srv/herdr/publish-index.sh
#
# It scans every repository under REPO_ROOT, reads their herdr-plugin.toml
# files, and commits herdr-index.json into INDEX_REPO when it changed.
# Clients then add:
#
#   [[marketplace]]
#   name = "internal"
#   kind = "git-index"
#   url  = "ssh://git@git.internal/srv/git/herdr/index.git"
set -eu

REPO_ROOT="${REPO_ROOT:-/srv/git}"                         # bare repositories live here
URL_BASE="${URL_BASE:-ssh://git@git.internal/srv/git}"      # how clients reach them
INDEX_REPO="${INDEX_REPO:-/srv/git/herdr/index.git}"        # bare repo holding the catalog
NAME="${NAME:-internal}"
HPM="${HPM:-herdr-pm}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if [ ! -d "$INDEX_REPO" ]; then
  git init -q --bare -b main "$INDEX_REPO"
fi
git clone -q "$INDEX_REPO" "$work/index" 2>/dev/null

"$HPM" index build --scan-dir "$REPO_ROOT" --url-base "$URL_BASE" --name "$NAME" \
  -o "$work/index/herdr-index.json"

cd "$work/index"
git add herdr-index.json
# generated_unix changes every run; only commit when the plugins changed.
if git diff --cached --quiet -- herdr-index.json 2>/dev/null; then
  exit 0
fi
if git rev-parse -q --verify HEAD >/dev/null &&
   [ "$(git show HEAD:herdr-index.json | sed '/generated_unix/d')" = "$(sed '/generated_unix/d' herdr-index.json)" ]; then
  exit 0
fi
git -c user.name=herdr-index -c user.email=herdr-index@localhost commit -q -m "Update plugin index"
git push -q origin HEAD:main
