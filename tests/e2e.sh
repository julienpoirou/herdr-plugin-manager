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

T="$(mktemp -d -t hpm-e2e.XXXXXX)"
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
out="$("$HPM" logs team.hello)"
contains "$out" "rollback"
ok "history records changes"

# --- auto-update: disabled plugins too, no preview, then off again
expect "auto-update enable on a disabled plugin" "$HPM" auto-update enable team.b
out="$("$HPM" info team.b)"
contains "$out" "auto-update"
contains "$out" "on"
sed -i.bak 's/0.2.0/0.3.0/g' "$T/work/mono/plugins/b/herdr-plugin.toml" && rm -f "$T/work/mono/plugins/b/herdr-plugin.toml.bak"
git -C "$T/work/mono" commit -q -am "b 0.3.0"
git -C "$T/work/mono" push -q "$SRV/team/mono.git" main
out="$("$HPM" auto-update < /dev/null 2>&1)"
contains "$out" "Updated team.b 0.3.0"
out="$("$HPM" list)"
contains "$out" "○ team.b"
ok "auto-update applies without a preview and keeps the plugin disabled"
out="$("$HPM" auto-update --hook event 2>&1)"
[ -z "$out" ] || fail "the event hook updated with trigger = startup: $out"
CFG="$("$HPM" config path)"
mkdir -p "$(dirname "$CFG")"
printf '[auto_update]\ntrigger = "min-interval"\nmin_interval = "1h"\n' > "$CFG"
out="$("$HPM" auto-update --hook event 2>&1)"
[ -z "$out" ] || fail "min-interval ran again within the interval: $out"
REG="$XDG_STATE_HOME/herdr/plugins/julienpoirou.herdr-plugin-manager/registry.json"
sed -i.bak 's/"last_auto_update": [0-9]*/"last_auto_update": 0/' "$REG" && rm -f "$REG.bak"
out="$("$HPM" auto-update --hook event 2>&1)"
contains "$out" "team.b"
rm -f "$CFG"
ok "auto-update hooks follow the trigger"
expect "auto-update disable" "$HPM" auto-update disable team.b
out="$("$HPM" auto-update 2>&1)"
contains "$out" "No plugin has auto-update turned on"
expect_fail "auto-update refuses an unknown plugin" "$HPM" auto-update enable does.not.exist

# --- abbreviated commit ids remain checkable
SHORT="$(git -C "$T/work/hello" rev-parse --short HEAD)"
expect "switch to an abbreviated commit" "$HPM" switch team.hello "$SHORT" --yes
expect "check updates after abbreviated commit" "$HPM" outdated


# --- annotated tags peel to commits and deleted refs leave the mirror
git -C "$T/work/hello" tag -a v-temp -m temporary
git -C "$T/work/hello" push -q "$HELLO" refs/tags/v-temp
expect "switch to an annotated tag" "$HPM" switch team.hello v-temp --yes
git -C "$T/work/hello" tag -d v-temp >/dev/null
git -C "$T/work/hello" push -q "$HELLO" :refs/tags/v-temp
expect_fail "deleted tag is pruned from the mirror" "$HPM" switch team.hello v-temp --yes
expect "restore default branch after tag test" "$HPM" unpin team.hello

# --- a corrupt private registry must not masquerade as local links
REG="$XDG_STATE_HOME/herdr/plugins/julienpoirou.herdr-plugin-manager/registry.json"
cp "$REG" "$T/registry.good"
printf 'not json\n' > "$REG"
expect_fail "corrupt registry is reported" "$HPM" list
mv "$T/registry.good" "$REG"

# --- Git for Windows defaults to core.autocrlf=true: checkouts must still
# --- carry the committed bytes, or the reviewed-manifest check fails
plugin "$T/work/crlf" team.crlf Crlf 0.1.0
publish "$T/work/crlf" crlf
expect "install with core.autocrlf=true" env GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.autocrlf GIT_CONFIG_VALUE_0=true "$HPM" install "$SRV/team/crlf.git" --yes
root="$("$HPM" info team.crlf | sed -n 's/^root  *//p')"
[ -f "$root/herdr-plugin.toml" ] || fail "no manifest under $root"
if grep -q "$(printf '\r')" "$root/herdr-plugin.toml"; then fail "checkout rewrote line endings"; fi
ok "checkouts keep the committed line endings"
"$HPM" uninstall team.crlf --yes >/dev/null

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

# --- path traversal and hostile Git transports
expect_fail "rejects parent components in --subdir" "$HPM" install "$MONO" --subdir ../hello --yes
MARKER="$T/ext-transport-ran"
expect_fail "rejects ext transport" env GIT_ALLOW_PROTOCOL=ext "$HPM" install "ext::touch $MARKER" --yes
[ ! -e "$MARKER" ] || fail "ext transport executed before confirmation"
ok "hostile transport executes nothing"

# --- terminal control sequences cannot rewrite the trust preview
plugin "$T/work/ansi" team.ansi Ansi 0.1.0
cat >> "$T/work/ansi/herdr-plugin.toml" <<'EOF'

[[events]]
on = "worktree.created\u001b[2K\r"
command = ["sh", "-c", "true"]

[[link_handlers]]
id = "web\u001b[2K"
title = "Open"
pattern = "^https://example.test/\u001b]0;disguised\u0007"
action = "hello"
EOF
publish "$T/work/ansi" ansi
out="$("$HPM" install "$SRV/team/ansi.git" --yes 2>&1 || true)"
contains "$out" "Plugin install preview"
case "$out" in *"$(printf '\033')"*) fail "escape sequence reached the terminal" ;; esac
ok "manifest escape sequences are sanitized"

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

[[marketplace]]
name = "herdr.dev"
enabled = false

[[marketplace]]
name = "gitlab.com"
enabled = false

[[marketplace]]
name = "codeberg.org"
enabled = false
EOF
out="$("$HPM" sources)"
contains "$out" "e2e"
out="$("$HPM" search)"
if printf '%s' "$out" | grep -q "herdr.dev\|gitlab.com\|codeberg.org"; then fail "a disabled built-in source was searched"; fi
contains "$out" "herdr-pm install file://$SRV/team/mono.git//plugins/b"
ok "git-index marketplace lists monorepo plugins with subdirs"
out="$("$HPM" search B --json)"
contains "$out" '"plugin_id": "team.b"'
ok "search filters"

# --- aliases
"$HPM" uninstall team.b --yes >/dev/null
expect "install through an alias" "$HPM" install "srv:team/mono.git//plugins/b" --yes

# --- git daemon transport (optional)
if git daemon --help >/dev/null 2>&1; then
  port=$((20000 + $$ % 20000))
  git daemon --reuseaddr --base-path="$SRV" --export-all --port="$port" "$SRV" >/dev/null 2>&1 &
  DAEMON_PID=$!
  # Wait until the daemon accepts connections (a fixed sleep raced on CI).
  tries=0
  until git ls-remote "git://127.0.0.1:$port/team/mono.git" >/dev/null 2>&1; do
    tries=$((tries + 1))
    [ "$tries" -lt 50 ] || fail "git daemon did not start"
    sleep 0.2
  done
  "$HPM" uninstall team.a --yes >/dev/null
  expect "install over git://" "$HPM" install "git://127.0.0.1:$port/team/mono.git//plugins/a" --yes
fi

# --- pinning the installed commit records it without reinstalling
CUR="$("$HPM" info team.a | sed -n 's/^commit  *//p')"
out="$("$HPM" pin team.a "$CUR" < /dev/null 2>&1)"
contains "$out" "Pinned team.a"
case "$out" in *preview*) fail "pinning the installed commit reinstalled it" ;; esac
contains "$("$HPM" info team.a)" "pinned"
expect "unpin" "$HPM" unpin team.a

# --- delete completely
CFG="$XDG_CONFIG_HOME/herdr/plugins/config/team.a"
mkdir -p "$CFG" && touch "$CFG/settings.toml"
expect "purge a git-managed plugin" "$HPM" uninstall team.a --purge --yes
[ ! -e "$CFG" ] || fail "config directory survived --purge"
ok "purge deletes the plugin's config directory"
plugin "$T/dev/gone" team.gone Gone 0.1.0
herdr plugin link "$T/dev/gone" >/dev/null
expect "purge a local link" "$HPM" uninstall team.gone --purge --yes
[ ! -e "$T/dev/gone" ] || fail "linked directory survived --purge"
ok "purge deletes a local link's directory"

# --- clean removes leftovers of removed plugins only, one or all
GHOST="$XDG_CONFIG_HOME/herdr/plugins/config/team.ghost"
OTHER="$XDG_CONFIG_HOME/herdr/plugins/config/team.other"
KEEP="$XDG_CONFIG_HOME/herdr/plugins/config/team.hello"
mkdir -p "$GHOST" "$OTHER" "$KEEP" "$XDG_STATE_HOME/herdr/plugins/team.ghost"
expect "clean one" "$HPM" clean team.ghost --yes
[ ! -e "$GHOST" ] || fail "clean kept the named plugin's config"
[ ! -e "$XDG_STATE_HOME/herdr/plugins/team.ghost" ] || fail "clean kept the named plugin's state"
[ -d "$OTHER" ] || fail "clean team.ghost deleted another removed plugin's config"
if "$HPM" clean team.hello --yes >/dev/null 2>&1; then fail "clean accepted an installed plugin"; fi
expect "clean all" "$HPM" clean --yes
[ ! -e "$OTHER" ] || fail "clean without ids kept a removed plugin's config"
[ -d "$KEEP" ] || fail "clean deleted an installed plugin's config"
[ -d "$XDG_STATE_HOME/herdr/plugins/julienpoirou.herdr-plugin-manager" ] || fail "clean deleted the manager's own data"
ok "clean deletes only what the chosen removed plugins left"

# --- explicit identity migration from a disabled v0.15 installation
# The old installation paths include another plugin; none may move or be deleted.
PROJECT="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
(
  export XDG_CONFIG_HOME="$T/migration-cfg" XDG_STATE_HOME="$T/migration-state"
  export HERDR_PM_CLI_DIR="$T/migration-cli"
  OLDHOME="$XDG_STATE_HOME/herdr/plugins/herdr-plugin-manager"
  NEWHOME="$XDG_STATE_HOME/herdr/plugins/julienpoirou.herdr-plugin-manager"
  OLDCFG="$XDG_CONFIG_HOME/herdr/plugins/config/herdr-plugin-manager"
  NEWCFG="$XDG_CONFIG_HOME/herdr/plugins/config/julienpoirou.herdr-plugin-manager"
  OLDROOT="$OLDHOME/installs/manager/src"
  EDITOR="$OLDHOME/installs/editor/src"
  plugin "$OLDROOT" herdr-plugin-manager Manager 0.15.0
  plugin "$EDITOR" ttt.editor Editor 1.0.0
  printf 'untouched\n' > "$EDITOR/keep.txt"
  herdr plugin link "$OLDROOT" --disabled >/dev/null
  herdr plugin link "$EDITOR" >/dev/null
  plugin "$T/work/manager" herdr-plugin-manager Manager 0.15.0
  publish "$T/work/manager" manager
  OLD_SHA="$(git -C "$T/work/manager" rev-parse HEAD)"
  mkdir -p "$OLDCFG"
  cat > "$T/migration-config.toml" <<EOF
logos = false
cli_on_path = false
agent_skill = false
history = "ecs"
[auto_update]
strategy = "latest"
trigger = "startup"
min_interval = "3h"
[auto_update.plugins]
"herdr-plugin-manager" = "latest"
EOF
  ln -s "$T/migration-config.toml" "$OLDCFG/config.toml"
  cat > "$T/migration-registry.json" <<EOF
{"version":1,"plugins":{"herdr-plugin-manager":{"id":"herdr-plugin-manager","name":"Manager","url":"$SRV/team/manager.git","requested_ref":"main","current":{"commit":"$OLD_SHA","version":"0.15.0","plugin_root":"$OLDROOT","install_dir":"$OLDHOME/installs/manager","requested_ref":"main","installed_unix":1},"previous":[],"first_installed_unix":1},"ttt.editor":{"id":"ttt.editor","name":"Editor","url":"$HELLO","requested_ref":"main","current":{"commit":"$OLD_SHA","version":"1.0.0","plugin_root":"$EDITOR","install_dir":"$OLDHOME/installs/editor","requested_ref":"main","installed_unix":2},"previous":[],"first_installed_unix":2}},"auto_update":["herdr-plugin-manager","ttt.editor"],"pins":{"herdr-plugin-manager":"$OLD_SHA","ttt.editor":"kept"},"last_auto_update":42}
EOF
  ln -s "$T/migration-registry.json" "$OLDHOME/registry.json"
  printf '{"unix":1,"action":"install","plugin_id":"herdr-plugin-manager","ok":true}\n' > "$OLDHOME/history.jsonl"
  cat > "$XDG_CONFIG_HOME/herdr/config.toml" <<EOF
[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "herdr-plugin-manager.open"
EOF
  expect_fail "legacy state requires explicit migration" "$HPM" list
  expect "migration dry-run" "$HPM" migrate "$PROJECT" --binary "$HPM" --dry-run
  [ ! -e "$NEWHOME/registry.json" ] || fail "migration dry-run wrote a registry"
  [ ! -e "$NEWCFG" ] || fail "migration dry-run moved config"
  expect "migrate disabled legacy manager" "$HPM" migrate "$PROJECT" --binary "$HPM" --yes
  [ -L "$NEWHOME/registry.json" ] || fail "migration replaced registry symlink"
  [ -L "$NEWCFG/config.toml" ] || fail "migration replaced config symlink"
  [ "$(cat "$EDITOR/keep.txt")" = untouched ] || fail "migration changed unrelated plugin files"
  contains "$(cat "$OLDROOT/herdr-plugin.toml")" 'id = "herdr-plugin-manager"'
  contains "$(cat "$NEWCFG/config.toml")" 'icons = false'
  contains "$(cat "$NEWCFG/config.toml")" 'min_interval = "3h"'
  contains "$(cat "$XDG_CONFIG_HOME/herdr/config.toml")" 'julienpoirou.herdr-plugin-manager.open'
  contains "$(cat "$NEWHOME/registry.json")" '"last_auto_update": 42'
  contains "$(cat "$NEWHOME/registry.json")" "\"ttt.editor\": \"kept\""
  SNAPSHOT_SHA="$(git -C "$PROJECT" rev-parse HEAD)"
  contains "$(cat "$NEWHOME/registry.json")" "\"julienpoirou.herdr-plugin-manager\": \"$SNAPSHOT_SHA\""
  out="$("$HPM" info julienpoirou.herdr-plugin-manager --json)"
  contains "$out" '"enabled": "false"'
  contains "$("$HPM" logs)" 'herdr-plugin-manager'
  expect "repeat migration is harmless" "$HPM" migrate "$PROJECT" --binary "$HPM" --yes
  expect_fail "legacy rollback cannot relink an old-id manifest" "$HPM" rollback julienpoirou.herdr-plugin-manager --yes
  expect "clean retains referenced legacy installation roots" "$HPM" clean --yes
  [ -f "$EDITOR/keep.txt" ] && [ -f "$OLDROOT/herdr-plugin.toml" ] || fail "clean deleted retained installation roots"
  plugin "$T/work/manager" julienpoirou.herdr-plugin-manager Manager 0.16.0
  git -C "$T/work/manager" commit -q -am "canonical identity"
  git -C "$T/work/manager" push -q "$SRV/team/manager.git" main
  expect "migrated manager updates from its preserved git remote" "$HPM" update julienpoirou.herdr-plugin-manager --yes
  contains "$("$HPM" info julienpoirou.herdr-plugin-manager)" '0.16.0'
  [ "$(cat "$EDITOR/keep.txt")" = untouched ] || fail "updating manager touched editor"
  printf '%s\n' "$pass" > "$T/migration-pass"
)
pass="$(cat "$T/migration-pass")"

# --- opt-in continuous local backup observes external edits, never commits
(
  export XDG_CONFIG_HOME="$T/backup-cfg" XDG_STATE_HOME="$T/backup-state"
  "$HPM" install "$HELLO" --yes >/dev/null 2>&1 || fail "backup fixture install"
  CFG="$XDG_CONFIG_HOME/herdr/plugins/config/julienpoirou.herdr-plugin-manager/config.toml"
  PLUGINCFG="$XDG_CONFIG_HOME/herdr/plugins/config/team.hello"
  SNAP="$XDG_STATE_HOME/herdr/plugins/julienpoirou.herdr-plugin-manager/backup"
  mkdir -p "$(dirname "$CFG")" "$PLUGINCFG"
  printf 'value = "one"\n' > "$PLUGINCFG/custom.toml"
  printf 'secret = "not-selected"\n' > "$PLUGINCFG/private.toml"
  printf '[backup]\nenabled = true\nfiles = ["plugins/team.hello/custom.toml"]\n' > "$CFG"
  trap 'printf "[backup]\nenabled = false\n" > "$CFG"; "$HPM" config show >/dev/null 2>&1 || true' EXIT
  expect "start continuous local backup" "$HPM" list
  tries=0
  until [ -f "$SNAP/plugins/team.hello/custom.toml" ] && [ "$(cat "$SNAP/plugins/team.hello/custom.toml")" = 'value = "one"' ]; do
    tries=$((tries + 1)); [ "$tries" -lt 15 ] || fail "backup did not capture initial selected file"
    sleep 1
  done
  printf 'value = "two"\n' > "$PLUGINCFG/custom.toml"
  tries=0
  until [ "$(cat "$SNAP/plugins/team.hello/custom.toml")" = 'value = "two"' ]; do
    tries=$((tries + 1)); [ "$tries" -lt 15 ] || fail "backup did not observe external config edit"
    sleep 1
  done
  [ ! -e "$SNAP/plugins/team.hello/private.toml" ] || fail "backup copied an unapproved plaintext file"
  if git -C "$SNAP" rev-parse --verify HEAD >/dev/null 2>&1; then fail "continuous backup committed without push"; fi
  printf '[backup]\nenabled = false\n' > "$CFG"
  expect "stop continuous local backup" "$HPM" config show
  trap - EXIT
  ok "external edits are captured locally and plaintext approval is enforced"
  printf '%s\n' "$pass" > "$T/backup-pass"
)
pass="$(cat "$T/backup-pass")"

# --- with a live server
herdr server >/dev/null 2>&1 &
SERVER_PID=$!
sleep 2
expect "disable (server)" "$HPM" disable team.hello
expect "enable (server)" "$HPM" enable team.hello

# Pull changes Herdr config through the live reload API, not a handoff/restart.
git init -q --bare --initial-branch=main "$T/live-backup.git"
mkdir -p "$T/live-manager-config" "$XDG_CONFIG_HOME/herdr"
[ -f "$XDG_CONFIG_HOME/herdr/config.toml" ] || printf 'onboarding = false\n' > "$XDG_CONFIG_HOME/herdr/config.toml"
cat > "$T/live-manager-config/config.toml" <<EOF
[backup]
enabled = false
directory = "$T/live-snapshot"
remote = "$T/live-backup.git"
files = ["herdr/config.toml"]
EOF
expect "push live configuration" env HERDR_PM_CONFIG_DIR="$T/live-manager-config" "$HPM" push --yes
git clone -q "$T/live-backup.git" "$T/live-editor"
printf '\n# synchronized live configuration\n' >> "$T/live-editor/herdr/config.toml"
git -C "$T/live-editor" add herdr/config.toml
git -C "$T/live-editor" commit -q -m "change live config"
git -C "$T/live-editor" push -q
expect "pull and reload live configuration" env HERDR_PM_CONFIG_DIR="$T/live-manager-config" "$HPM" pull --yes
contains "$(cat "$XDG_CONFIG_HOME/herdr/config.toml")" '# synchronized live configuration'
kill -0 "$SERVER_PID" || fail "pull replaced or stopped the running Herdr server"
ok "GitOps applies live config without replacing the server"

expect "doctor" sh -c "'$HPM' doctor || true"
expect "uninstall" "$HPM" uninstall team.hello --yes
out="$("$HPM" list)"
case "$out" in *team.hello*) fail "team.hello still listed" ;; esac
ok "uninstalled plugins disappear"

echo "all $pass checks passed"
