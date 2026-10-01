#!/usr/bin/env bash
# The install socket unit (peridot-install.socket), where the daemon asks
# for an install: installed and recorded like the other units, restarted
# when its file was written (it listens where the file says), replaced only
# while it is Peridot's, stopped with its instances and removed on uninstall.
source "$T/lib.sh"
fresh2() { fresh; SU="$HOME/.config/systemd/user/peridot-install.socket"; }

case_ "1. fresh install writes and records the socket unit, and restarts the socket"
fresh2; r=$(inst)
expect "exit 0" [ "$r" = 0 ] &&
expect "unit" same "$ROOT/dist/peridot-install.socket" "$SU" &&
expect "recorded" manifest_lists "$SU" && manifest_verifies &&
expect "path named" said "$SU" &&
expect "reloaded before the restart" [ "$(grep -n 'daemon-reload' "$FAKE_LOG" | head -1 | cut -d: -f1)" -lt "$(grep -n 'restart peridot-install.socket' "$FAKE_LOG" | head -1 | cut -d: -f1)" ] && ok

case_ "2. reinstall replaces it silently and restarts it; an edited unit is kept, named, and not restarted"
: >"$FAKE_LOG"; r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "nothing kept" not said "Keeping your $SU" && expect "restarted" logged "restart peridot-install.socket" && manifest_verifies && ok
echo "# mine" >>"$SU"; cp "$SU" "$HOME/mine"; : >"$FAKE_LOG"; r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "kept" same "$HOME/mine" "$SU" && expect "said" said "Keeping your $SU" && expect "not restarted" not_logged "restart peridot-install.socket" && ok

case_ "3. a socket unit that isn't Peridot's, a link, or one provided elsewhere stops the install before anything is written"
fresh2; mkdir -p "$(dirname "$SU")"; printf '[Socket]\nListenStream=/tmp/x\n' >"$SU"; r=$(inst)
expect "stop" [ "$r" = 1 ] && said "$SU exists and isn't Peridot's" && expect "no binary" [ ! -e "$BIN/peridotd" ] && ok
fresh2; mkdir -p "$(dirname "$SU")"; ln -s /dev/null "$SU"; r=$(inst)
expect "stop" [ "$r" = 1 ] && said "masked or linked" && [ "$(readlink "$SU")" = /dev/null ] && ok
fresh2; FAKE_SOCKET_FRAGMENT="/etc/systemd/user/peridot-install.socket" r=$(inst)
expect "stop" [ "$r" = 1 ] && said "already provided by /etc/systemd/user/peridot-install.socket" && [ ! -e "$SU" ] && ok

case_ "4. uninstall stops the socket and its instances, removes Peridot's unit and reloads; an edited one is stopped but kept; one from elsewhere is left alone"
fresh2; inst >/dev/null; : >"$FAKE_LOG"; FAKE_INSTANCES="peridot-install@1-a.service" r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "gone" [ ! -e "$SU" ] &&
expect "socket stopped" logged "stop peridot-install.socket" && expect "instances stopped" logged "stop peridot-install@1-a.service" &&
expect "stopped before removal" [ "$(grep -n 'stop peridot-install.socket' "$FAKE_LOG" | head -1 | cut -d: -f1)" -lt "$(grep -n 'daemon-reload' "$FAKE_LOG" | tail -1 | cut -d: -f1)" ] && ok
fresh2; inst >/dev/null; echo "# mine" >>"$SU"; : >"$FAKE_LOG"; r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "kept" [ -f "$SU" ] && said "$SU is kept: you changed it" && logged "stop peridot-install.socket" && ok
fresh2; inst >/dev/null; : >"$FAKE_LOG"; FAKE_SOCKET_FRAGMENT="/etc/systemd/user/peridot-install.socket" r=$(uninst)
expect "left alone" [ -f "$SU" ] && said "comes from /etc/systemd/user/peridot-install.socket" && not_logged "stop peridot-install.socket" && ok
case_ "5. an edited socket that listens elsewhere, or whose template runs something else, is left running with its instances"
for v in "FAKE_SOCKET_LISTEN=/tmp/other.sock (Stream)" "FAKE_SOCKET_ACCEPT=no" "FAKE_INSTALL_EXECSTART={ path=/usr/bin/other ; argv[]=/usr/bin/other }"; do
  fresh2; inst >/dev/null; echo "# mine" >>"$SU"; : >"$FAKE_LOG"; r=$(export "$v"; uninst)
  expect "exit 0" [ "$r" = 0 ] && expect "kept" [ -f "$SU" ] && expect "socket not stopped ($v)" not_logged "stop peridot-install.socket" &&
  said "no longer starts Peridot's install script" || break
done && ok
fresh2; inst >/dev/null; : >"$FAKE_LOG"; FAKE_INSTANCES="peridot-install@1-a.service" FAKE_INSTALL_EXECSTART="{ path=/usr/bin/other ; argv[]=/usr/bin/other }" r=$(uninst)
expect "Peridot's own files removed" [ ! -e "$SU" ] && expect "instances of a repurposed template not stopped" not_logged "stop peridot-install@1-a.service" && ok

case_ "6. a running instance with its own drop-in running something else is not stopped; Peridot's own instances are"
fresh2; inst >/dev/null; : >"$FAKE_LOG"
FAKE_INSTANCES="peridot-install@1-a.service peridot-install@2-b.service" FAKE_ROGUE_INSTANCE="peridot-install@2-b.service" r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "ours stopped" logged "stop peridot-install@1-a.service" &&
expect "rogue not stopped" not_logged "stop peridot-install@2-b.service" && said "peridot-install@2-b.service runs something other than" &&
expect "never by pattern" not_logged "stop peridot-install@*" && ok

finish
