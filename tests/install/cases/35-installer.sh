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
# they were run with and the whole environment they got.
b64url() { printf '%s' "$1" | base64 -w0 | tr '+/' '-_' | tr -d '='; }
fake_omarchy() {
  local c; mkdir -p "$HOME/.local/share/omarchy/bin"
  for c in omarchy-theme-install omarchy-plugin-add omarchy-theme-set; do
    printf '#!/bin/sh\necho "$0 $*" >>"%s"\nenv | sort >"%s"\nexit "$(cat "%s" 2>/dev/null || echo 0)"\n' "$HOME/ran.log" "$HOME/env.log" "$HOME/rc" >"$HOME/.local/share/omarchy/bin/$c"
    chmod +x "$HOME/.local/share/omarchy/bin/$c"
  done
  : >"$HOME/ran.log"
}
# run_script <instance>: as the unit would run it, with the session-ish
# environment plus a secret that must not get through.
run_script() {
  (cd / && env -i HOME="$HOME" USER=tester XDG_RUNTIME_DIR="$HOME/rt" OMARCHY_PATH="$HOME/.local/share/omarchy" \
      PATH="$HOME/evil-bin:/usr/bin" WAYLAND_DISPLAY=wayland-1 DISPLAY=:0 DBUS_SESSION_BUS_ADDRESS="unix:path=$HOME/rt/bus" \
      LANG=en_US.UTF-8 PERIDOT_SECRET=hunter2 SSH_AUTH_SOCK=/nope HYPRLAND_INSTANCE_SIGNATURE=abc123 \
      bash "$ROOT/dist/peridot-install" "$@" >"$HOME/out.txt" 2>&1); echo $?
}
ran() { grep -qF -- "$1" "$HOME/ran.log"; }
nothing_ran() { [[ ! -s $HOME/ran.log ]]; }

case_ "6. the script refuses anything but a well-formed request, and runs nothing then"
fresh2; fake_omarchy; mkdir -p "$HOME/evil-bin"; printf '#!/bin/sh\necho EVIL >>"%s"\n' "$HOME/ran.log" >"$HOME/evil-bin/omarchy-theme-install"; chmod +x "$HOME/evil-bin/omarchy-theme-install"
bad=(
  ""                                                    # no argument
  "theme"                                               # no encoding
  "theme:"                                              # empty
  "mixtape:$(b64url https://github.com/a/b)"            # unknown kind
  "theme:!!!"                                           # not base64url
  "theme:$(b64url http://github.com/a/b)"               # not https
  "theme:$(b64url https://example.com/a/b)"             # not a host Peridot installs from
  "theme:$(b64url 'https://github.com/a/b;rm -rf ~')"   # characters outside the set
  "theme:$(b64url 'https://github.com/a/b c')"          # a space
  "plugin:$(b64url "https://github.com/$(printf 'a%.0s' {1..300})")"   # too long
  "theme-set:$(b64url 'Tokyo Night')"                   # not a theme name
  "theme-set:$(b64url '../escape')"
  "theme-set:$(b64url "$(printf 'a%.0s' {1..65})")"     # too long a name
)
okay=1
for req in "${bad[@]}"; do
  if [[ -z $req ]]; then r=$(run_script); else r=$(run_script "$req"); fi
  if [[ $r != 2 ]] || ! nothing_ran; then fail "accepted or ran something for: ${req:0:60} (exit $r)"; okay=0; fi
done
r=$(run_script "theme:$(b64url https://github.com/a/b)" extra)
[[ $r == 2 ]] && nothing_ran || { fail "two arguments accepted"; okay=0; }
(( okay )) && ok

case_ "7. a theme address runs omarchy-theme-install from Omarchy's bin with a clean environment"
fresh2; fake_omarchy; r=$(run_script "theme:$(b64url https://github.com/acme/omarchy-sea-theme.git)")
expect "exit 0" [ "$r" = 0 ] &&
expect "ran the right command" ran "$HOME/.local/share/omarchy/bin/omarchy-theme-install https://github.com/acme/omarchy-sea-theme.git" &&
expect "secret not passed" not grep -q PERIDOT_SECRET "$HOME/env.log" &&
expect "SSH_AUTH_SOCK not passed" not grep -q SSH_AUTH_SOCK "$HOME/env.log" &&
expect "PATH is Omarchy's, not the inherited one" grep -qx "PATH=$HOME/.local/share/omarchy/bin:/usr/local/bin:/usr/bin:/bin" "$HOME/env.log" &&
expect "session variables kept" grep -qx "WAYLAND_DISPLAY=wayland-1" "$HOME/env.log" && grep -qx "DISPLAY=:0" "$HOME/env.log" &&
  grep -qx "DBUS_SESSION_BUS_ADDRESS=unix:path=$HOME/rt/bus" "$HOME/env.log" && grep -qx "HYPRLAND_INSTANCE_SIGNATURE=abc123" "$HOME/env.log" &&
expect "identity kept" grep -qx "HOME=$HOME" "$HOME/env.log" && grep -qx "USER=tester" "$HOME/env.log" && grep -qx "OMARCHY_PATH=$HOME/.local/share/omarchy" "$HOME/env.log" &&
expect "nothing else" [ "$(grep -cvE '^(HOME|USER|LOGNAME|XDG_RUNTIME_DIR|TERM|LANG|DBUS_SESSION_BUS_ADDRESS|WAYLAND_DISPLAY|DISPLAY|HYPRLAND_INSTANCE_SIGNATURE|OMARCHY_PATH|PATH|PWD|SHLVL|_)=' "$HOME/env.log")" = 0 ] && ok

case_ "8. a plugin runs omarchy-plugin-add --yes (confirmed in the panel already; no terminal here); a theme name runs omarchy-theme-set; the command's exit status is the script's"
fresh2; fake_omarchy; r=$(run_script "plugin:$(b64url https://gitlab.com/acme/omarchy-weather)")
expect "exit 0" [ "$r" = 0 ] && expect "plugin-add --yes" ran "omarchy-plugin-add https://gitlab.com/acme/omarchy-weather --yes" && ok
r=$(run_script "theme-set:$(b64url tokyo-night)")
expect "exit 0" [ "$r" = 0 ] && expect "theme-set" ran "omarchy-theme-set tokyo-night" && ok
echo 7 >"$HOME/rc"; r=$(run_script "theme-set:$(b64url tokyo-night)")
expect "exit status passed on" [ "$r" = 7 ] && ok

case_ "9. a codeberg address with the odd but allowed characters decodes exactly"
fresh2; fake_omarchy; url='https://codeberg.org/a.b_c/d~e%20f+g:h/i-j.git'; r=$(run_script "theme:$(b64url "$url")")
expect "exit 0" [ "$r" = 0 ] && expect "exact argument" ran "omarchy-theme-install $url" && ok

finish
