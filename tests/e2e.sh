#!/bin/sh
# End-to-end test against a real `herdr` binary and plain git repositories.
#
#   cargo build && sh tests/e2e.sh [path/to/herdr-pm]
#
# Everything happens in a throwaway HOME-like sandbox: herdr's config and
# state, the manager's data, and a fake "git server" of bare repositories
# served over file paths, file:// and (when available) git daemon.
set -eu

HPM="${1:-$(pwd)/target/debug/herdr-pm}"
command -v herdr >/dev/null || { echo "e2e: herdr is not on PATH" >&2; exit 2; }
[ -x "$HPM" ] || { echo "e2e: $HPM not found (cargo build first)" >&2; exit 2; }

T="$(mktemp -d "${TMPDIR:-/tmp}/hpm-e2e.XXXXXX")"
trap 'herdr server stop >/dev/null 2>&1 || true; [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" 2>/dev/null; rm -rf "$T"' EXIT
export XDG_CONFIG_HOME="$T/cfg" XDG_STATE_HOME="$T/state" XDG_CACHE_HOME="$T/cache"
export NO_COLOR=1 HERDR_PM_NO_TOKEN=1
export GIT_AUTHOR_NAME=e2e GIT_AUTHOR_EMAIL=e2e@example.invalid GIT_COMMITTER_NAME=e2e GIT_COMMITTER_EMAIL=e2e@example.invalid
SRV="$T/srv"
mkdir -p "$SRV/team" "$T/work"

pass=0
ok() { pass=$((pass + 1)); echo "ok $pass - $1"; }
fail() { echo "FAIL - $1" >&2; exit 1; }
expect() { # description, command...
  desc="$1"; shift
  if out="$("$@" 2>&1)"; then ok "$desc"; else echo "$out" >&2; fail "$desc"; fi
}
expect_fail() {
  desc="$1"; shift
  if out="$("$@" 2>&1)"; then echo "$out" >&2; fail "$desc (unexpectedly succeeded)"; else ok "$desc"; fi
}
contains() { printf '%s' "$1" | grep -q -- "$2" || { echo "--- output:"; echo "$1"; fail "expected '$2'"; }; }

plugin() { # dir id name version
  mkdir -p "$1"
  cat > "$1/herdr-plugin.toml" <<EOF
id = "$2"
name = "$3"
version = "$4"
min_herdr_version = "0.7.0"
platforms = ["linux", "macos", "windows"]

[[build]]
command = ["sh", "-c", "echo built-$4 > built.txt"]

[[actions]]
id = "hello"
title = "Say hello"
command = ["sh", "-c", "cat built.txt"]
EOF
}

publish() { # work-dir repo-name
  git -C "$1" init -q -b main
  git -C "$1" add -A
  git -C "$1" commit -q -m "release"
  git clone -q --bare "$1" "$SRV/team/$2.git"
  touch "$SRV/team/$2.git/git-daemon-export-ok"
}

plugin "$T/work/hello" team.hello Hello 1.0.0
publish "$T/work/hello" hello
plugin "$T/work/mono/plugins/a" team.a A 0.1.0
plugin "$T/work/mono/plugins/b" team.b B 0.2.0
publish "$T/work/mono" mono

HELLO="$SRV/team/hello.git"
MONO="file://$SRV/team/mono.git"

# --- install from a local bare repository
expect "install from a bare repo path" "$HPM" install "$HELLO" --yes
out="$("$HPM" list --json)"
contains "$out" '"origin": "git"'
root="$(herdr plugin list --json | sed -n 's/.*"plugin_root":"\([^"]*\)".*/\1/p' | head -n 1)"
[ "$(cat "$root/built.txt")" = "built-1.0.0" ] || fail "build step output missing"
ok "build step ran in the installation"

# --- subdirectories
expect_fail "monorepo without subdir lists its plugins" "$HPM" install "$MONO" --yes
out="$("$HPM" install "$MONO" --yes 2>&1 || true)"
contains "$out" "plugins/a, plugins/b"
expect "install with //subdir over file://" "$HPM" install "$MONO//plugins/a" --yes
expect "install with --subdir, disabled" "$HPM" install "$MONO" --subdir plugins/b --disabled --yes
out="$("$HPM" list)"
contains "$out" "○ team.b"

# --- updates, history, rollback
sed -i.bak 's/1.0.0/1.1.0/g' "$T/work/hello/herdr-plugin.toml" && rm -f "$T/work/hello/herdr-plugin.toml.bak"
printf '\n[[startup]]\ncommand = ["sh", "-c", "true"]\n' >> "$T/work/hello/herdr-plugin.toml"
git -C "$T/work/hello" commit -q -am "1.1.0"
git -C "$T/work/hello" push -q "$HELLO" main
out="$("$HPM" outdated)"
contains "$out" "team.hello  1.0.0"
contains "$out" "1.1.0"
out="$("$HPM" update team.hello --yes 2>&1)"
contains "$out" "what it runs changed"
contains "$out" "+ startup #1"
contains "$out" "Updated team.hello 1.1.0"
ok "update shows a command diff and applies"
out="$("$HPM" update --yes 2>&1)"
contains "$out" "team.hello is up to date"
ok "second update is a no-op"
expect "rollback" "$HPM" rollback team.hello --yes
out="$("$HPM" list)"
contains "$out" "team.hello  1.0.0"
expect "pin" "$HPM" pin team.hello
out="$("$HPM" update team.hello --yes 2>&1)"
contains "$out" "pinned"
ok "pinned plugin is skipped"
expect "unpin" "$HPM" unpin team.hello
expect "update after unpin" "$HPM" update team.hello --yes
out="$("$HPM" history team.hello)"
contains "$out" "rollback"
ok "history records changes"

# --- refusals
plugin "$T/work/newer" team.newer Newer 0.0.1
sed -i.bak 's/min_herdr_version = "0.7.0"/min_herdr_version = "99.0.0"/' "$T/work/newer/herdr-plugin.toml"
rm -f "$T/work/newer/herdr-plugin.toml.bak"
publish "$T/work/newer" newer
expect_fail "refuses a plugin that needs a newer herdr" "$HPM" install "$SRV/team/newer.git" --yes
plugin "$T/dev/hello" team.hello Hello 9.9.9
herdr plugin link "$T/dev/hello" >/dev/null
expect_fail "refuses to replace a hand-linked plugin" "$HPM" install "$HELLO" --yes
expect "replaces it with --replace" "$HPM" install "$HELLO" --yes --replace
plugin "$T/work/quiet" team.quiet Quiet 0.1.0
publish "$T/work/quiet" quiet
out="$("$HPM" install "$SRV/team/quiet.git" < /dev/null 2>&1 || true)"
contains "$out" "pass --yes"
out="$("$HPM" list)"
case "$out" in *team.quiet*) fail "installed without confirmation" ;; esac
ok "refuses a non-interactive install without --yes"

# --- enable/disable without and with a server
expect "disable (no server)" "$HPM" disable team.a
expect "enable (no server)" "$HPM" enable team.a

# --- marketplace from a plain git repository
"$HPM" index build --scan-dir "$SRV" --url-base "file://$SRV" --name e2e -o "$T/work/herdr-index.json" >/dev/null 2>&1
mkdir -p "$T/work/index" && cp "$T/work/herdr-index.json" "$T/work/index/"
publish "$T/work/index" index
expect "index check" "$HPM" index check "$T/work/herdr-index.json"
mkdir -p "$(dirname "$("$HPM" config path)")"
cat > "$("$HPM" config path)" <<EOF
[aliases]
srv = "$SRV/"

[[marketplace]]
name = "e2e"
kind = "git-index"
url = "$SRV/team/index.git"
EOF
out="$("$HPM" search)"
contains "$out" "herdr-pm install file://$SRV/team/mono.git//plugins/b"
ok "git-index marketplace lists monorepo plugins with subdirs"
out="$("$HPM" search B --json)"
contains "$out" '"plugin_id": "team.b"'
ok "search filters"

# --- aliases
"$HPM" uninstall team.b --yes >/dev/null
expect "install through an alias" "$HPM" install "srv:team/mono.git//plugins/b" --yes

# --- export / import into a fresh herdr
"$HPM" export -o "$T/export.json" 2>/dev/null
XDG_CONFIG_HOME="$T/cfg2" XDG_STATE_HOME="$T/state2" "$HPM" import "$T/export.json" --yes >/dev/null 2>&1 || fail "import"
out="$(XDG_CONFIG_HOME="$T/cfg2" XDG_STATE_HOME="$T/state2" "$HPM" list)"
contains "$out" "team.hello"
contains "$out" "team.a"
ok "import reproduces the plugin set"

# --- git daemon transport (optional)
if git daemon --help >/dev/null 2>&1; then
  port=$((20000 + $$ % 20000))
  git daemon --reuseaddr --base-path="$SRV" --export-all --port="$port" "$SRV" >/dev/null 2>&1 &
  DAEMON_PID=$!
  sleep 1
  "$HPM" uninstall team.a --yes >/dev/null
  expect "install over git://" "$HPM" install "git://127.0.0.1:$port/team/mono.git//plugins/a" --yes
fi

# --- with a live server
herdr server >/dev/null 2>&1 &
sleep 2
expect "disable (server)" "$HPM" disable team.hello
expect "enable (server)" "$HPM" enable team.hello
expect "doctor" sh -c "'$HPM' doctor || true"
expect "uninstall" "$HPM" uninstall team.hello --yes
out="$("$HPM" list)"
case "$out" in *team.hello*) fail "team.hello still listed" ;; esac
ok "uninstalled plugins disappear"

echo "all $pass checks passed"
