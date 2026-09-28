#!/usr/bin/env bash
# The bus proxy unit (peridot-dbus-proxy.service) peridot.service is bound
# to: installed and recorded like the service, in place before the service
# is started, replaced only while it is Peridot's, stopped and removed on
# uninstall.
source "$T/lib.sh"
fresh2() { fresh; PU="$HOME/.config/systemd/user/peridot-dbus-proxy.service"; }

case_ "1. fresh install writes and records the proxy unit before the service is enabled"
fresh2; r=$(inst)
expect "exit 0" [ "$r" = 0 ] &&
expect "unit" same "$ROOT/dist/peridot-dbus-proxy.service" "$PU" &&
expect "recorded" manifest_lists "$PU" && manifest_verifies &&
expect "path named" said "$PU" && expect "service enabled" logged "enable --now" && ok

case_ "2. reinstall replaces it silently and reloads; an edited unit is kept and named"
r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "nothing kept" not said "Keeping your $PU" && expect "reloaded" logged "daemon-reload" && manifest_verifies && ok
echo "# mine" >>"$PU"; cp "$PU" "$HOME/mine"; r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "kept" same "$HOME/mine" "$PU" && expect "said" said "Keeping your $PU" && ok

case_ "3. a proxy unit that isn't Peridot's, a link, or one provided elsewhere stops the install before anything is written"
fresh2; mkdir -p "$(dirname "$PU")"; printf '[Service]\nExecStart=/usr/bin/true\n' >"$PU"; r=$(inst)
expect "stop" [ "$r" = 1 ] && said "$PU exists and isn't Peridot's" && expect "no binary" [ ! -e "$BIN/peridotd" ] && ok
fresh2; mkdir -p "$(dirname "$PU")"; ln -s /dev/null "$PU"; r=$(inst)
expect "stop" [ "$r" = 1 ] && said "masked or linked" && [ "$(readlink "$PU")" = /dev/null ] && ok
fresh2; FAKE_PROXY_FRAGMENT="/etc/systemd/user/peridot-dbus-proxy.service" r=$(inst)
expect "stop" [ "$r" = 1 ] && said "already provided by /etc/systemd/user/peridot-dbus-proxy.service" && [ ! -e "$PU" ] && ok

case_ "4. uninstall stops and removes Peridot's proxy unit and reloads; an edited one is stopped but kept; one from elsewhere is left alone"
fresh2; inst >/dev/null; : >"$FAKE_LOG"; r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "gone" [ ! -e "$PU" ] &&
expect "stopped" logged "stop peridot-dbus-proxy.service" && expect "reloaded" logged "daemon-reload" && ok
fresh2; inst >/dev/null; echo "# mine" >>"$PU"; : >"$FAKE_LOG"; r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "kept" [ -f "$PU" ] && said "$PU is kept: you changed it" && logged "stop peridot-dbus-proxy.service" && ok
fresh2; inst >/dev/null; : >"$FAKE_LOG"; FAKE_PROXY_FRAGMENT="/etc/systemd/user/peridot-dbus-proxy.service" r=$(uninst)
expect "left alone" [ -f "$PU" ] && said "comes from /etc/systemd/user/peridot-dbus-proxy.service" && not_logged "stop peridot-dbus-proxy.service" && ok

finish
