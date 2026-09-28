#!/usr/bin/env bash
# The install script (~/.local/bin/peridot-install) and its template unit
# (peridot-install@.service): installed and recorded like the binaries and
# the service, replaced only while they are Peridot's, removed on uninstall;
# and the script itself: what it refuses, what it runs, with what environment.
source "$T/lib.sh"
# fresh() sets HOME and BIN; these follow them.
fresh2() { fresh; U2="$HOME/.config/systemd/user/peridot-install@.service"; S="$BIN/peridot-install"; }

case_ "1. fresh install writes and records the script and the template unit"
fresh2; r=$(inst)
expect "exit 0" [ "$r" = 0 ] &&
expect "script" same "$ROOT/dist/peridot-install" "$S" && expect "executable" [ -x "$S" ] &&
expect "unit" same "$ROOT/dist/peridot-install@.service" "$U2" &&
expect "recorded" manifest_lists "$S" && manifest_lists "$U2" && manifest_verifies &&
expect "paths named" said "$S" && said "$U2" && ok

case_ "2. reinstall replaces both silently; an edited template unit is kept and named"
r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "nothing kept" not said "Keeping your $U2" && expect "still recorded" manifest_verifies && ok
echo "# mine" >>"$U2"; cp "$U2" "$HOME/mine"; r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "kept" same "$HOME/mine" "$U2" && expect "said" said "Keeping your $U2" && ok

case_ "3. a template unit that isn't Peridot's, or a link, stops the install before anything is written"
fresh2; mkdir -p "$(dirname "$U2")"; printf '[Service]\nExecStart=/usr/bin/true\n' >"$U2"; r=$(inst)
expect "stop" [ "$r" = 1 ] && said "$U2 exists and isn't Peridot's" && expect "no binary" [ ! -e "$BIN/peridotd" ] && ok
fresh2; mkdir -p "$(dirname "$U2")"; ln -s /dev/null "$U2"; r=$(inst)
expect "stop" [ "$r" = 1 ] && said "masked or linked" && [ "$(readlink "$U2")" = /dev/null ] && ok

case_ "4. an unrecorded peridot-install stops a non-interactive run; --replace-existing replaces it with a backup"
fresh2; printf 'theirs\n' >"$S"; chmod +x "$S"; r=$(inst)
expect "stop" [ "$r" = 1 ] && said "$S exists but isn't recorded" && said "--replace-existing=$S" && [ "$(cat "$S")" = theirs ] && ok
r=$(inst "--replace-existing=$S")
expect "exit 0" [ "$r" = 0 ] && expect "replaced" same "$ROOT/dist/peridot-install" "$S" &&
expect "backup" grep -q theirs "$B"/peridot-install.* && expect "recorded" manifest_verifies && ok

case_ "5. uninstall removes both when they are Peridot's, stops instances, reloads; an edited unit stays"
fresh2; inst >/dev/null; r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "script gone" [ ! -e "$S" ] && expect "unit gone" [ ! -e "$U2" ] &&
expect "instances stopped" logged "stop peridot-install@*.service" && expect "reloaded" logged "daemon-reload" && ok
fresh2; inst >/dev/null; echo "# mine" >>"$U2"; printf 'theirs\n' >"$S"; r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "unit kept" [ -f "$U2" ] && said "$U2 is kept: you changed it" &&
expect "script kept" [ "$(cat "$S")" = theirs ] && said "$S isn't recorded" && ok

# ── The script itself ───────────────────────────────────────────────────
# A fake Omarchy in the test home (the script honours OMARCHY_PATH only for
# /usr/share/omarchy or ~/.local/share/omarchy), whose commands log what
# they were run with and the whole environment they got, print a line when
# told to fail (rc in $HOME/rc), and exit with it.
b64url() { printf '%s' "$1" | base64 -w0 | tr '+/' '-_' | tr -d '='; }
fake_omarchy() {
  local c; mkdir -p "$HOME/.local/share/omarchy/bin"
  for c in omarchy-theme-install omarchy-plugin-add omarchy-theme-set; do
    printf '#!/bin/sh\necho "$0 $*" >>"%s"\nenv | sort >"%s"\nrc="$(cat "%s" 2>/dev/null || echo 0)"\necho "fake: working on it"\n[ "$rc" = 0 ] || echo "fake: it broke (rc $rc)"\nexit "$rc"\n' \
      "$HOME/ran.log" "$HOME/env.log" "$HOME/rc" >"$HOME/.local/share/omarchy/bin/$c"
    chmod +x "$HOME/.local/share/omarchy/bin/$c"
  done
  : >"$HOME/ran.log"
}
# run_script [request]: as the unit would run it, with the session-ish
# environment plus a secret that must not get through; the request goes in
# on stdin (the socket), the reply comes out on stdout, the rest to stderr
# (the journal). Prints the exit status; the reply is in $HOME/reply.txt.
run_script() {
  (cd / && printf '%s' "${1:+$1
}" | env -i HOME="$HOME" USER=tester XDG_RUNTIME_DIR="$HOME/rt" OMARCHY_PATH="$HOME/.local/share/omarchy" \
      PATH="$HOME/evil-bin:/usr/bin" WAYLAND_DISPLAY=wayland-1 DISPLAY=:0 DBUS_SESSION_BUS_ADDRESS="unix:path=$HOME/rt/bus" \
      LANG=en_US.UTF-8 PERIDOT_SECRET=hunter2 SSH_AUTH_SOCK=/nope HYPRLAND_INSTANCE_SIGNATURE=abc123 \
      bash "$ROOT/dist/peridot-install" >"$HOME/reply.txt" 2>"$HOME/out.txt"); echo $?
}
reply() { cat "$HOME/reply.txt"; }
reply_is() { [[ "$(reply)" == "$1" ]]; }
reply_starts() { [[ "$(reply)" == "$1"* ]]; }
one_reply_line() { [[ "$(wc -l <"$HOME/reply.txt")" == 1 ]]; }
ran() { grep -qF -- "$1" "$HOME/ran.log"; }
nothing_ran() { [[ ! -s $HOME/ran.log ]]; }
# consent <kind:arg> [seconds ago] [mode]: what the panel writes.
CONSENT_DIR=
consent() {
  local text=$1 ago=${2:-0} mode=${3:-600} id
  CONSENT_DIR="$HOME/.local/state/peridot/consent"; mkdir -p -m 700 "$CONSENT_DIR"
  id="$(printf '%s' "$text" | sha256sum | cut -d' ' -f1)"
  printf '%s\n%s\n' "$text" "$(( $(date +%s) - ago ))" >"$CONSENT_DIR/$id"; chmod "$mode" "$CONSENT_DIR/$id"
  CONSENT_FILE="$CONSENT_DIR/$id"
}
CONSENT_FILE=

case_ "6. the script refuses anything but a well-formed request line, replies so, and runs nothing then"
fresh2; fake_omarchy; mkdir -p "$HOME/evil-bin"; printf '#!/bin/sh\necho EVIL >>"%s"\n' "$HOME/ran.log" >"$HOME/evil-bin/omarchy-theme-install"; chmod +x "$HOME/evil-bin/omarchy-theme-install"
consent "theme:https://github.com/a/b"   # even with consent for the sane version of some of these
bad=(
  ""                                                    # nothing on stdin
  "theme"                                               # no encoding
  "theme:"                                              # empty
  "mixtape:$(b64url https://github.com/a/b)"            # unknown kind
  "theme:!!!"                                           # not base64url
  "theme:$(b64url http://github.com/a/b)"               # not https
  "theme:$(b64url https://example.com/a/b)"             # not a host Peridot installs from
  "theme:$(b64url 'https://github.com/a/b;rm -rf ~')"   # characters outside the set
  "theme:$(b64url 'https://github.com/a/b c')"          # a space
  "plugin:$(b64url "https://github.com/$(printf 'a%.0s' {1..300})")"   # too long an address
  "theme:$(b64url "https://github.com/$(printf 'a%.0s' {1..200})")x$(printf 'A%.0s' {1..400})"   # a line past 512 bytes
  "theme-set:$(b64url 'Tokyo Night')"                   # not a theme name
  "theme-set:$(b64url '../escape')"
  "theme-set:$(b64url "$(printf 'a%.0s' {1..65})")"     # too long a name
)
okay=1
for req in "${bad[@]}"; do
  r=$(run_script "$req")
  if [[ $r != 2 ]] || ! reply_is "error: not an install request" || ! one_reply_line || ! nothing_ran; then
    fail "accepted, or ran something, or an odd reply for: ${req:0:60} (exit $r, reply: $(reply | head -c 80))"; okay=0
  fi
done
(( okay )) && expect "consent untouched by refusals" [ -f "$CONSENT_FILE" ] && ok

case_ "7. no consent record: refused, nothing runs; a record for something else, a stale one, one from the future, one others can read: the same"
fresh2; fake_omarchy; req="theme:$(b64url https://github.com/acme/omarchy-sea-theme.git)"
r=$(run_script "$req")
expect "exit 2" [ "$r" = 2 ] && expect "reply" reply_starts "error: not confirmed in Peridot's panel (" && expect "nothing ran" nothing_ran && ok
consent "theme:https://github.com/acme/omarchy-other-theme.git"; r=$(run_script "$req")
expect "other argument refused" [ "$r" = 2 ] && reply_starts "error: not confirmed in Peridot's panel (" && nothing_ran && [ -f "$CONSENT_FILE" ] && ok
consent "plugin:https://github.com/acme/omarchy-sea-theme.git"; r=$(run_script "$req")
expect "other kind refused" [ "$r" = 2 ] && reply_starts "error: not confirmed in Peridot's panel (" && nothing_ran && ok
consent "theme:https://github.com/acme/omarchy-sea-theme.git" 700; r=$(run_script "$req")
expect "stale (700 s) refused" [ "$r" = 2 ] && reply_starts "error: not confirmed in Peridot's panel (" && nothing_ran && ok
consent "theme:https://github.com/acme/omarchy-sea-theme.git" -120; r=$(run_script "$req")
expect "from the future refused" [ "$r" = 2 ] && reply_starts "error: not confirmed in Peridot's panel (" && nothing_ran && ok
consent "theme:https://github.com/acme/omarchy-sea-theme.git" 0 644; r=$(run_script "$req")
expect "readable by others refused" [ "$r" = 2 ] && reply_starts "error: not confirmed in Peridot's panel (" && nothing_ran && ok
rm -f "$CONSENT_FILE"; mkdir "$CONSENT_FILE"; r=$(run_script "$req")
expect "a folder in its place refused" [ "$r" = 2 ] && reply_starts "error: not confirmed in Peridot's panel (" && nothing_ran && ok

case_ "8. a confirmed theme address runs omarchy-theme-install from Omarchy's bin with a clean environment, answers ok, and the record is used up"
fresh2; fake_omarchy; consent "theme:https://github.com/acme/omarchy-sea-theme.git"
r=$(run_script "theme:$(b64url https://github.com/acme/omarchy-sea-theme.git)")
expect "exit 0" [ "$r" = 0 ] && expect "reply ok" reply_is ok && one_reply_line &&
expect "ran the right command" ran "$HOME/.local/share/omarchy/bin/omarchy-theme-install https://github.com/acme/omarchy-sea-theme.git" &&
expect "record consumed" [ ! -e "$CONSENT_FILE" ] &&
expect "command output went to stderr" grep -q "fake: working on it" "$HOME/out.txt" &&
expect "secret not passed" not grep -q PERIDOT_SECRET "$HOME/env.log" &&
expect "SSH_AUTH_SOCK not passed" not grep -q SSH_AUTH_SOCK "$HOME/env.log" &&
expect "PATH is Omarchy's, not the inherited one" grep -qx "PATH=$HOME/.local/share/omarchy/bin:/usr/local/bin:/usr/bin:/bin" "$HOME/env.log" &&
expect "session variables kept" grep -qx "WAYLAND_DISPLAY=wayland-1" "$HOME/env.log" && grep -qx "DISPLAY=:0" "$HOME/env.log" &&
  grep -qx "DBUS_SESSION_BUS_ADDRESS=unix:path=$HOME/rt/bus" "$HOME/env.log" && grep -qx "HYPRLAND_INSTANCE_SIGNATURE=abc123" "$HOME/env.log" &&
expect "identity kept" grep -qx "HOME=$HOME" "$HOME/env.log" && grep -qx "USER=tester" "$HOME/env.log" && grep -qx "OMARCHY_PATH=$HOME/.local/share/omarchy" "$HOME/env.log" &&
expect "nothing else" [ "$(grep -cvE '^(HOME|USER|LOGNAME|XDG_RUNTIME_DIR|TERM|LANG|DBUS_SESSION_BUS_ADDRESS|WAYLAND_DISPLAY|DISPLAY|HYPRLAND_INSTANCE_SIGNATURE|OMARCHY_PATH|PATH|PWD|SHLVL|_)=' "$HOME/env.log")" = 0 ] && ok
r=$(run_script "theme:$(b64url https://github.com/acme/omarchy-sea-theme.git)")
expect "the same request again is refused (record used)" [ "$r" = 2 ] && reply_starts "error: not confirmed" && ok

case_ "9. a confirmed plugin runs omarchy-plugin-add --yes; a theme name runs omarchy-theme-set; a failing command gives its last line and status"
fresh2; fake_omarchy; consent "plugin:https://gitlab.com/acme/omarchy-weather"; r=$(run_script "plugin:$(b64url https://gitlab.com/acme/omarchy-weather)")
expect "exit 0" [ "$r" = 0 ] && reply_is ok && expect "plugin-add --yes" ran "omarchy-plugin-add https://gitlab.com/acme/omarchy-weather --yes" && ok
consent "theme-set:tokyo-night"; r=$(run_script "theme-set:$(b64url tokyo-night)")
expect "exit 0" [ "$r" = 0 ] && reply_is ok && expect "theme-set" ran "omarchy-theme-set tokyo-night" && ok
echo 7 >"$HOME/rc"; consent "theme-set:tokyo-night"; r=$(run_script "theme-set:$(b64url tokyo-night)")
expect "exit status passed on" [ "$r" = 7 ] && expect "last line as the error" reply_is "error: fake: it broke (rc 7)" && one_reply_line && ok

case_ "10. a codeberg address with the odd but allowed characters decodes exactly"
fresh2; fake_omarchy; url='https://codeberg.org/a.b_c/d~e%20f+g:h/i-j.git'; consent "theme:$url"; r=$(run_script "theme:$(b64url "$url")")
expect "exit 0" [ "$r" = 0 ] && reply_is ok && expect "exact argument" ran "omarchy-theme-install $url" && ok

finish
