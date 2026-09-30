# Shared by install.sh and uninstall.sh. Sourced, not run.
#
# The one rule: Peridot replaces or removes a path only if it is a regular file
# (never a symlink, never a folder) whose SHA-256 matches either the line for
# that path in Peridot's install record, or a file some Peridot version
# wrote (dist/known-hashes.tsv). Every replacement swaps the file out first and
# checks the bytes that came out, so an edit made after the check is kept,
# not lost. Everything else is yours: it stays, and the output says so.
#
# Record: ${XDG_STATE_HOME:-~/.local/state}/peridot/installed.tsv, one
# "<sha256><TAB><absolute path>" per file Peridot wrote, plus
# "backup<TAB><original><TAB><backup>" for files moved aside with your
# consent, and "menu<TAB><file><TAB><sha256>" per line Peridot added to
# Omarchy's Share menu file (a file it edits, but doesn't own). Backups under state/peridot/backup/ are never deleted by Peridot.
#
# What stays imprecise, on purpose (documented in the README):
# parent folders are resolved the way the OS does (a symlinked ~/.local/bin
# is followed; only the final path component is never a link); a file you
# deliberately set back to an older Peridot version's exact bytes counts as
# Peridot's; when `mv --exchange` isn't available the swap degrades to two
# renames with a microsecond gap; hard links elsewhere keep the old bytes.

[[ -n ${BASH_VERSION:-} ]] || { echo "dist/lib.sh needs bash" >&2; return 1 2>/dev/null || exit 1; }

PERIDOT_URL="https://github.com/derekross/peridot"
MARKER=".installed-by-peridot"

CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
CACHE_HOME="${XDG_CACHE_HOME:-$HOME/.cache}"
STATE_HOME="${XDG_STATE_HOME:-$HOME/.local/state}"

BINDIR="$HOME/.local/bin"
UNITDIR="$CONFIG_HOME/systemd/user"            # systemd honours XDG_CONFIG_HOME
UNIT="$UNITDIR/peridot.service"
INSTALL_UNIT="$UNITDIR/peridot-install@.service"   # runs Omarchy installs outside the daemon's sandbox
PROXY_UNIT="$UNITDIR/peridot-dbus-proxy.service"   # the session bus, filtered, for peridotd (it is bound to this)
SOCKET_UNIT="$UNITDIR/peridot-install.socket"      # where the daemon asks for an install; each connection starts an instance of INSTALL_UNIT
INSTALLER="$BINDIR/peridot-install"                # what that template unit runs
PLUGINDIR="$HOME/.config/omarchy/plugins"      # where Omarchy itself looks
MENU="$HOME/.config/omarchy/extensions/omarchy-menu.jsonc"   # Omarchy's, not XDG
STATEDIR="$STATE_HOME/peridot"
MANIFEST="$STATEDIR/installed.tsv"
BACKUPDIR="$STATEDIR/backup"
DATADIR="$DATA_HOME/peridot"                   # as opal-core's AppDirs::PERIDOT
CONFIGDIR="$CONFIG_HOME/peridot"
CACHEDIR="$CACHE_HOME/peridot"

PLUGIN_ID="$(jq -r .id manifest.json 2>/dev/null || true)"
[[ $PLUGIN_ID =~ ^[a-z0-9][a-z0-9.-]{0,63}$ ]] || { echo "manifest.json has no usable plugin id" >&2; exit 1; }
PLUGIN_PATH="$PLUGINDIR/$PLUGIN_ID"

say() { printf '%s\n' "$*"; }
note() { printf '  %s\n' "$*"; }
die() { printf '%s\n' "$*" >&2; exit 1; }

# ── Files ──────────────────────────────────────────────────────────────
path_kind() {
  if [[ -L $1 ]]; then echo symlink
  elif [[ ! -e $1 ]]; then echo missing
  elif [[ -f $1 ]]; then echo file
  elif [[ -d $1 ]]; then echo dir
  else echo other
  fi
}
# SHA-256 of a regular file, or nothing. Never fails the caller.
file_hash() {
  local out
  out="$(sha256sum -- "$1" 2>/dev/null)" || out=
  printf '%s\n' "${out%% *}"
}
describe_file() {
  local size when
  size="$(stat -c %s -- "$1" 2>/dev/null || echo '?')"
  when="$(date -r "$1" '+%Y-%m-%d %H:%M' 2>/dev/null || echo '?')"
  printf '%s bytes, modified %s\n' "$size" "$when"
}
# One-line, shell-safe spelling of a name for messages.
q() { printf '%q' "$1"; }

# Temp files this run made; removed on exit. Never anything of yours.
TEMPS=()
cleanup_temps() { local t; for t in "${TEMPS[@]}"; do [[ -e $t || -L $t ]] && rm -rf -- "$t"; done; return 0; }
trap cleanup_temps EXIT

MV_EXCHANGE=0
mv --help 2>/dev/null | grep -q -- '--exchange' && MV_EXCHANGE=1

# ── The install record ─────────────────────────────────────────────────
declare -A MANIFEST_HASH=()   # absolute path -> sha256
BACKUP_LINES=()               # "original<TAB>backup"
declare -A MENU_LINE_HASH=()  # "<menu file><TAB><sha256 of one line Peridot wrote>" -> 1
load_manifest() {
  MANIFEST_HASH=(); BACKUP_LINES=(); MENU_LINE_HASH=()
  [[ -f $MANIFEST && ! -L $MANIFEST ]] || return 0
  local h p extra
  while IFS=$'\t' read -r h p extra || [[ -n $h ]]; do
    if [[ $h == backup ]]; then
      [[ $p == /* && $extra == /* ]] && BACKUP_LINES+=("$p"$'\t'"$extra")
    elif [[ $h == menu ]]; then
      [[ $p == /* && $extra =~ ^[0-9a-f]{64}$ ]] && MENU_LINE_HASH["$p"$'\t'"$extra"]=1
    elif [[ $h =~ ^[0-9a-f]{64}$ && $p == /* && -z $extra ]]; then
      MANIFEST_HASH["$p"]=$h
    else
      note "ignoring an odd line in $MANIFEST: ${h:0:20}…"
    fi
  done <"$MANIFEST"
}
write_manifest() {
  local tmp p b k
  if (( ${#MANIFEST_HASH[@]} == 0 && ${#BACKUP_LINES[@]} == 0 && ${#MENU_LINE_HASH[@]} == 0 )); then
    rm -f -- "$MANIFEST"
    return 0
  fi
  mkdir -p -m 700 -- "$STATEDIR"
  tmp="$(mktemp -- "$STATEDIR/.installed.XXXXXX")"
  {
    for p in "${!MANIFEST_HASH[@]}"; do printf '%s\t%s\n' "${MANIFEST_HASH[$p]}" "$p"; done | LC_ALL=C sort -t $'\t' -k2
    for b in "${BACKUP_LINES[@]}"; do printf 'backup\t%s\n' "$b"; done
    for k in "${!MENU_LINE_HASH[@]}"; do printf 'menu\t%s\n' "$k"; done | LC_ALL=C sort
  } >"$tmp"
  chmod 600 -- "$tmp"
  mv -T -- "$tmp" "$MANIFEST"
}
record() { MANIFEST_HASH["$1"]="$(file_hash "$1")"; write_manifest; }
unrecord() { [[ -n ${MANIFEST_HASH[$1]:-} ]] || return 0; unset 'MANIFEST_HASH[$1]'; write_manifest; }

# The state folder must be a real, writable folder before anything is written.
# Also takes the lock, so two installs (two checkouts, say) can't interleave.
prepare_state() {
  if [[ -L $STATEDIR && ! -d $STATEDIR ]]; then die "$STATEDIR is a dangling link; fix it first."; fi
  [[ -e $STATEDIR && ! -d $STATEDIR ]] && die "$STATEDIR exists and isn't a folder; move it aside first."
  mkdir -p -m 700 -- "$STATEDIR" || die "can't create $STATEDIR"
  [[ -w $STATEDIR ]] || die "$STATEDIR isn't writable."
  exec 9>>"$STATEDIR/.lock"
  flock -n 9 || die "another Peridot install or uninstall is running (lock: $STATEDIR/.lock)."
}

# ── What Peridot versions wrote ───────────────────────────────────────────
declare -A KNOWN=()   # "sha256<TAB>logical path" -> label
render_plugin_manifest() { jq '.entryPoints |= with_entries(.value |= ltrimstr("shell-plugin/"))' manifest.json; }
hash_stdin() { sha256sum | cut -d' ' -f1; }
load_known() {
  local h l logical f
  KNOWN=()
  [[ -f dist/known-hashes.tsv ]] || die "dist/known-hashes.tsv is missing; run this from an Peridot checkout."
  while IFS=$'\t' read -r h l logical || [[ -n $h ]]; do
    [[ $h == \#* || -z $logical ]] && continue
    KNOWN["$h"$'\t'"$logical"]=$l
  done <dist/known-hashes.tsv
  # This checkout counts too (an install made from it without a record).
  KNOWN["$(file_hash dist/peridot.service)"$'\t'unit]=checkout
  KNOWN["$(file_hash dist/peridot-install@.service)"$'\t'install-unit]=checkout
  KNOWN["$(file_hash dist/peridot-dbus-proxy.service)"$'\t'proxy-unit]=checkout
  KNOWN["$(file_hash dist/peridot-install.socket)"$'\t'install-socket]=checkout
  KNOWN["$(file_hash dist/peridot-install)"$'\t'bin/peridot-install]=checkout
  while IFS= read -r -d '' f; do
    [[ ${f#shell-plugin/} == manifest.json ]] && continue
    KNOWN["$(file_hash "$f")"$'\t'"plugin/${f#shell-plugin/}"]=checkout
  done < <(find shell-plugin -type f -print0)
  KNOWN["$(render_plugin_manifest | hash_stdin)"$'\t'plugin/manifest.json]=checkout
}

# The marker earlier installs left in the plugin copy: exactly the 0.1.x
# content (known by hash), or the URL followed by "<sha256>  <file>" lines,
# whose lines then count as a record for that folder. Anything else is
# your file.
declare -A MARKER_HASH=()
MARKER_STATE=none   # none | known | hashes | foreign
load_marker() {
  local dir=$1 m="$1/$MARKER" line first=1
  MARKER_HASH=(); MARKER_STATE=none
  [[ -e $m || -L $m ]] || return 0
  [[ -f $m && ! -L $m ]] || { MARKER_STATE=foreign; return 0; }
  if [[ -n ${KNOWN["$(file_hash "$m")"$'\t'"plugin/$MARKER"]:-} ]]; then MARKER_STATE=known; return 0; fi
  while IFS= read -r line || [[ -n $line ]]; do
    if (( first )); then
      first=0
      [[ $line == "$PERIDOT_URL" ]] || { MARKER_STATE=foreign; return 0; }
      continue
    fi
    if [[ $line =~ ^([0-9a-f]{64})\ \ ([^[:space:]/].*)$ ]]; then
      MARKER_HASH["$dir/${BASH_REMATCH[2]}"]=${BASH_REMATCH[1]}
    else
      MARKER_STATE=foreign; MARKER_HASH=(); return 0
    fi
  done <"$m"
  MARKER_STATE=hashes
}

# The only ownership test: owned_file <absolute path> <logical path>.
owned_file() {
  local p=$1 logical=$2 h
  [[ -f $p && ! -L $p ]] || return 1
  h="$(file_hash "$p")"
  [[ -n $h ]] || return 1
  [[ ${MANIFEST_HASH[$p]:-} == "$h" ]] && return 0
  [[ -n ${KNOWN["$h"$'\t'"$logical"]:-} ]] && return 0
  [[ ${MARKER_HASH[$p]:-} == "$h" ]] && return 0
  return 1
}

# ── Backups, replacing, removing ───────────────────────────────────────
# backup_file <path> <name> [original]: moves the file into the backup
# folder under a name nobody else has, records it (as <original> when the
# file was already swapped out of its place), and leaves the new path in
# BACKUP_DEST (not printed: a subshell couldn't update the record).
BACKUP_DEST=
backup_file() {
  local src=$1 name=$2 orig=${3:-$1} ts n=0 dest
  ts="$(date +%Y%m%dT%H%M%S)"
  mkdir -p -m 700 -- "$BACKUPDIR"
  while :; do
    dest="$BACKUPDIR/$name.$ts"; (( n )) && dest="$dest.$n"
    ( set -o noclobber; : >"$dest" ) 2>/dev/null && break
    (( n++ )); (( n > 999 )) && die "couldn't find a free backup name in $BACKUPDIR"
  done
  mv -T -- "$src" "$dest"
  BACKUP_LINES+=("$orig"$'\t'"$dest")
  write_manifest
  BACKUP_DEST=$dest
}

# replace_owned <source> <dest> <mode> <expected hash of what's at dest>
# With an empty expected hash, nothing must be at dest (link(2) refuses
# otherwise). With one, the new file is swapped in and the file that came
# out is checked: if it isn't the expected bytes any more (changed after
# the check, or a link that appeared), it is kept as a backup and said so.
replace_owned() {
  local src=$1 dest=$2 mode=$3 expect=$4 dir tmp out b
  dir="$(dirname -- "$dest")"
  mkdir -p -- "$dir"
  tmp="$(mktemp -- "$dir/.peridot.XXXXXX")"; TEMPS+=("$tmp")
  cp -- "$src" "$tmp"
  chmod "$mode" -- "$tmp"
  if [[ -z $expect ]]; then
    if ! ln -- "$tmp" "$dest" 2>/dev/null; then
      rm -f -- "$tmp"
      note "$dest appeared while installing; not written."
      return 1
    fi
    rm -f -- "$tmp"
  else
    if (( MV_EXCHANGE )) && mv --exchange -T -- "$tmp" "$dest" 2>/dev/null; then
      out=$tmp
    else
      # No atomic exchange: two renames, a moment with nothing at dest.
      out="$(mktemp -u -- "$dir/.peridot.XXXXXX")"
      mv -T -- "$dest" "$out" || { rm -f -- "$tmp"; note "couldn't move $dest aside; not replaced."; return 1; }
      mv -T -- "$tmp" "$dest"
    fi
    if [[ -L $out || "$(file_hash "$out")" != "$expect" ]]; then
      backup_file "$out" "$(basename -- "$dest")" "$dest"
      note "$dest changed after it was checked; what was there is kept in $BACKUP_DEST"
    else
      rm -f -- "$out"
    fi
  fi
  MANIFEST_HASH["$dest"]="$(file_hash "$dest")"
  write_manifest
}

# remove_owned <path> <expected hash>: moves the file aside, checks it, removes it.
remove_owned() {
  local p=$1 expect=$2 out b
  out="$(mktemp -u -- "$(dirname -- "$p")/.peridot.XXXXXX")"
  mv -T -- "$p" "$out" || { note "couldn't move $p aside; left as is."; return 1; }
  if [[ -L $out || "$(file_hash "$out")" != "$expect" ]]; then
    backup_file "$out" "$(basename -- "$p")" "$p"
    note "$p changed after it was checked; kept in $BACKUP_DEST"
  else
    rm -f -- "$out"
  fi
  unset 'MANIFEST_HASH[$p]'
  write_manifest
}

# Remove the folders that held <rel> inside <root>, while they're empty.
prune_dirs() {
  local root=$1 rel=$2
  rel="$(dirname -- "$rel")"
  while [[ $rel != . && $rel != / ]]; do
    rmdir -- "$root/$rel" 2>/dev/null || break
    rel="$(dirname -- "$rel")"
  done
}

# walk <dir>: every entry below it, relative, into WALK. Fails (and says
# why) if the folder can't be read fully; then nothing in it is touched.
WALK=()
walk() {
  local dir=$1 list err
  WALK=()
  list="$(mktemp)"; err="$(mktemp)"; TEMPS+=("$list" "$err")
  if ! find "$dir" -mindepth 1 -printf '%P\0' >"$list" 2>"$err"; then
    note "couldn't read all of $dir: $(head -c 200 -- "$err")"
    return 1
  fi
  mapfile -d '' -t WALK <"$list"
  rm -f -- "$list" "$err"
}

# ── The plugin copy ────────────────────────────────────────────────────
plugin_dir_state() {
  local k; k="$(path_kind "$1")"
  case $k in
    missing | symlink | other) echo "$k" ;;
    file) echo other ;;
    dir) if [[ -e $1/.git || -L $1/.git ]]; then echo checkout; else echo dir; fi ;;
  esac
}

# The files Peridot ships into a plugin copy: SHIPPED[rel] = source path.
declare -A SHIPPED=()
load_shipped() {
  local f rendered
  SHIPPED=()
  while IFS= read -r -d '' f; do
    [[ ${f#shell-plugin/} == manifest.json ]] && continue
    SHIPPED["${f#shell-plugin/}"]=$f
  done < <(find shell-plugin -type f -print0)
  rendered="$(mktemp)"; TEMPS+=("$rendered")
  render_plugin_manifest >"$rendered"
  SHIPPED[manifest.json]=$rendered
}

# Say "keeping …" once per top-level folder of yours, not once per file in it.
declare -A ANNOUNCED=()
keep_note() {
  local rel=$1 why=$2 top=${1%%/*}
  if [[ $rel == */* && -n ${ANNOUNCED[$top]:-} ]]; then return 0; fi
  if [[ $rel != */* && -d ${3:-}/$rel && ! -L ${3:-}/$rel ]]; then ANNOUNCED[$top]=1; note "keeping $(q "$rel")/: $why"; return 0; fi
  note "keeping $(q "$rel"): $why"
}

# update_plugin_copy <dir>: Peridot's files replaced or added in place, its
# files that are no longer shipped removed; everything of yours kept.
update_plugin_copy() {
  local dir=$1 rel kind h
  local -A done=() skip=()
  local -a pruned=()
  ANNOUNCED=()
  load_marker "$dir"
  walk "$dir" || { note "leaving $dir as it is."; return 1; }
  for rel in "${WALK[@]}"; do
    kind="$(path_kind "$dir/$rel")"
    case $kind in
      dir)
        if [[ -n ${SHIPPED[$rel]:-} ]]; then note "keeping $(q "$rel")/: a folder where Peridot ships a file (Peridot's isn't installed)"; skip[$rel]=1
        else keep_note "$rel" "not Peridot's" "$dir"; fi
        unrecord "$dir/$rel"
        continue ;;
      symlink | other)
        keep_note "$rel" "not Peridot's (a link)" "$dir"
        [[ -n ${SHIPPED[$rel]:-} ]] && { note "Peridot's $(q "$rel") isn't installed."; skip[$rel]=1; }
        unrecord "$dir/$rel"
        continue ;;
    esac
    if [[ $rel == "$MARKER" && ( $MARKER_STATE == known || $MARKER_STATE == hashes ) ]]; then
      remove_owned "$dir/$rel" "$(file_hash "$dir/$rel")"
      continue
    fi
    if owned_file "$dir/$rel" "plugin/$rel"; then
      h="$(file_hash "$dir/$rel")"
      if [[ -n ${SHIPPED[$rel]:-} ]]; then
        replace_owned "${SHIPPED[$rel]}" "$dir/$rel" 644 "$h" && done[$rel]=1
      else
        note "removing $(q "$rel"): Peridot no longer ships it"
        remove_owned "$dir/$rel" "$h" && pruned+=("$rel")
      fi
    elif [[ -n ${SHIPPED[$rel]:-} ]]; then
      note "keeping $(q "$rel"): you changed it (Peridot's version isn't installed)"
      unset 'MANIFEST_HASH[$dir/$rel]'; write_manifest
      skip[$rel]=1
    else
      keep_note "$rel" "not a file Peridot wrote, or you changed it" "$dir"
    fi
  done
  for rel in "${!SHIPPED[@]}"; do
    [[ -n ${done[$rel]:-} || -n ${skip[$rel]:-} ]] && continue
    replace_owned "${SHIPPED[$rel]}" "$dir/$rel" 644 "" || true
  done
  for rel in "${pruned[@]}"; do prune_dirs "$dir" "$rel"; done
}

# create_plugin_copy <dir>: a fresh copy where nothing was.
create_plugin_copy() {
  local dir=$1 rel
  mkdir -p -m 755 -- "$dir"
  for rel in "${!SHIPPED[@]}"; do replace_owned "${SHIPPED[$rel]}" "$dir/$rel" 644 "" || true; done
}

# remove_plugin_copy <dir>: Peridot's files go, yours stay. Returns 0 if the
# folder is gone afterwards.
remove_plugin_copy() {
  local dir=$1 rel kind h
  local -a pruned=()
  ANNOUNCED=()
  load_marker "$dir"
  walk "$dir" || { note "leaving $dir as it is."; return 1; }
  for rel in "${WALK[@]}"; do
    kind="$(path_kind "$dir/$rel")"
    case $kind in
      dir) keep_note "$rel" "not Peridot's" "$dir"; continue ;;
      symlink | other) keep_note "$rel" "not Peridot's (a link)" "$dir"; continue ;;
    esac
    if [[ $rel == "$MARKER" && ( $MARKER_STATE == known || $MARKER_STATE == hashes ) ]]; then
      remove_owned "$dir/$rel" "$(file_hash "$dir/$rel")"; continue
    fi
    if owned_file "$dir/$rel" "plugin/$rel"; then
      h="$(file_hash "$dir/$rel")"
      remove_owned "$dir/$rel" "$h" && pruned+=("$rel")
    else
      keep_note "$rel" "not a file Peridot wrote, or you changed it" "$dir"
    fi
  done
  for rel in "${pruned[@]}"; do prune_dirs "$dir" "$rel"; done
  if rmdir -- "$dir" 2>/dev/null; then return 0; fi
  note "kept $dir: it still holds entries that aren't Peridot's."
  return 1
}

# ── Everything else Peridot touches ───────────────────────────────────────
systemctl_user() {
  local out
  if out="$(systemctl --user "$@" 2>&1)"; then return 0; fi
  note "systemctl --user $*: ${out:-failed}"
  return 0
}
# ask <question> → 0 for yes. Only a terminal can say yes.
ask() {
  local reply
  [[ -t 0 ]] || return 1
  read -r -p "$1 [y/N] " reply </dev/tty || return 1
  [[ $reply =~ ^[Yy] ]]
}

# The unit, as systemd sees it and as it is on disk.
UNIT_KIND=missing UNIT_STATE=missing UNIT_FRAGMENT= UNIT_EXECSTART= UNIT_DROPIN=0
inspect_unit() {
  local show
  UNIT_KIND="$(path_kind "$UNIT")"
  UNIT_DROPIN=0; [[ -d $UNIT.d ]] && UNIT_DROPIN=1
  show="$(systemctl --user show -p FragmentPath -p ExecStart peridot.service 2>/dev/null || true)"
  UNIT_FRAGMENT="$(printf '%s\n' "$show" | awk -F= '$1 == "FragmentPath" { print substr($0, 14) }')"
  UNIT_EXECSTART="$(printf '%s\n' "$show" | awk -F= '$1 == "ExecStart" { print substr($0, 11) }')"
  case $UNIT_KIND in
    missing)
      if [[ -n $UNIT_FRAGMENT && $UNIT_FRAGMENT != /dev/null ]]; then UNIT_STATE=elsewhere; else UNIT_STATE=missing; fi ;;
    symlink) UNIT_STATE=symlink ;;
    dir | other) UNIT_STATE=other ;;
    file)
      if [[ -n $UNIT_FRAGMENT && $UNIT_FRAGMENT != "$UNIT" && $UNIT_FRAGMENT != /dev/null ]]; then UNIT_STATE=elsewhere
      elif owned_file "$UNIT" unit; then UNIT_STATE=owned
      elif grep -qF -- "$PERIDOT_URL" "$UNIT"; then UNIT_STATE=edited
      else UNIT_STATE=foreign; fi ;;
  esac
}
unit_runs_our_binary() { exec_only "$UNIT_EXECSTART" "$BINDIR/peridotd"; }
unit_is_active() { systemctl --user is-active --quiet peridot.service 2>/dev/null; }

# What a unit runs or listens on, as systemd has loaded it (drop-ins
# included). A unit is stopped only while that is Peridot's own program,
# never because its file mentions Peridot: a unit you repurposed keeps
# running.
RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
# unit_prop <unit> <property>: its value(s), one per line.
unit_prop() {
  systemctl --user show -p "$2" "$1" 2>/dev/null | awk -v k="$2" 'index($0, k "=") == 1 { print substr($0, length(k) + 2) }' || true
}
# exec_path <ExecStart as systemctl shows it>: the first command's program.
exec_path() { [[ $1 =~ \{\ path=([^\ \;}]+) ]] && printf '%s\n' "${BASH_REMATCH[1]}"; }
# exec_only <ExecStart> <program>: exactly one command, and it is <program>.
exec_only() {
  [[ $(grep -o '{ path=' <<<"$1" | wc -l) == 1 && "$(exec_path "$1")" == "$2" ]]
}
# The proxy: one xdg-dbus-proxy serving Peridot's bus socket.
proxy_serves_peridot() {
  local ex prog
  ex="$(unit_prop peridot-dbus-proxy.service ExecStart)"
  prog="$(exec_path "$ex")" || return 1
  [[ ${prog##*/} == xdg-dbus-proxy ]] && exec_only "$ex" "$prog" && [[ " $ex " == *" $RUNTIME_DIR/peridot/bus "* ]]
}
# The install template: runs Peridot's install script and nothing else.
install_unit_runs_our_script() { exec_only "$(unit_prop peridot-install@peridot.service ExecStart)" "$INSTALLER"; }
# The socket: listens only where the daemon asks, and each connection
# starts an instance of that template.
socket_serves_peridot() {
  [[ "$(unit_prop peridot-install.socket Listen)" == "$RUNTIME_DIR/peridot-install.sock (Stream)" &&
     "$(unit_prop peridot-install.socket Accept)" == yes ]] && install_unit_runs_our_script
}

# The other two units, the same way: aux_unit_state <path> <name systemd
# knows it by> <logical path> leaves the state in AUX_STATE (missing |
# owned | edited | foreign | symlink | elsewhere | other) and where systemd
# loads it from in AUX_FRAGMENT.
AUX_STATE=missing AUX_FRAGMENT=
aux_unit_state() {
  local path=$1 name=$2 logical=$3 show kind
  kind="$(path_kind "$path")"
  show="$(systemctl --user show -p FragmentPath "$name" 2>/dev/null || true)"
  AUX_FRAGMENT="$(printf '%s\n' "$show" | awk -F= '$1 == "FragmentPath" { print substr($0, 14) }')"
  case $kind in
    missing)
      if [[ -n $AUX_FRAGMENT && $AUX_FRAGMENT != /dev/null ]]; then AUX_STATE=elsewhere; else AUX_STATE=missing; fi ;;
    symlink) AUX_STATE=symlink ;;
    dir | other) AUX_STATE=other ;;
    file)
      if [[ -n $AUX_FRAGMENT && $AUX_FRAGMENT != "$path" && $AUX_FRAGMENT != /dev/null ]]; then AUX_STATE=elsewhere
      elif owned_file "$path" "$logical"; then AUX_STATE=owned
      elif grep -qF -- "$PERIDOT_URL" "$path"; then AUX_STATE=edited
      else AUX_STATE=foreign; fi ;;
  esac
}
# The install template (peridot-install@.service). systemctl show wants an
# instance; any name finds the template.
INSTALL_UNIT_STATE=missing INSTALL_UNIT_FRAGMENT=
inspect_install_unit() {
  aux_unit_state "$INSTALL_UNIT" 'peridot-install@peridot.service' install-unit
  INSTALL_UNIT_STATE=$AUX_STATE INSTALL_UNIT_FRAGMENT=$AUX_FRAGMENT
}
# The bus proxy (peridot-dbus-proxy.service) peridot.service is bound to.
PROXY_UNIT_STATE=missing PROXY_UNIT_FRAGMENT=
inspect_proxy_unit() {
  aux_unit_state "$PROXY_UNIT" peridot-dbus-proxy.service proxy-unit
  PROXY_UNIT_STATE=$AUX_STATE PROXY_UNIT_FRAGMENT=$AUX_FRAGMENT
}
# The socket (peridot-install.socket) the daemon asks for installs on.
SOCKET_UNIT_STATE=missing SOCKET_UNIT_FRAGMENT=
inspect_socket_unit() {
  aux_unit_state "$SOCKET_UNIT" peridot-install.socket install-socket
  SOCKET_UNIT_STATE=$AUX_STATE SOCKET_UNIT_FRAGMENT=$AUX_FRAGMENT
}

# binary_state <path>: missing | symlink | other | owned | unrecorded
binary_state() {
  case "$(path_kind "$1")" in
    missing) echo missing ;;
    symlink) echo symlink ;;
    dir | other) echo other ;;
    file) if owned_file "$1" "bin/$(basename -- "$1")"; then echo owned; else echo unrecorded; fi ;;
  esac
}

# ── The Share menu ─────────────────────────────────────────────────────
# Peridot can add "Private link" entries to Omarchy's Share menu. That file
# is yours (~/.config/omarchy/extensions/omarchy-menu.jsonc), so install.sh
# edits it only with your consent and keeps a backup; uninstall.sh takes
# out only lines that are still exactly the ones Peridot put in. Which
# lines those are is decided by their bytes, never by a marker: each line
# Peridot writes is recorded in the install record ("menu<TAB><file>
# <TAB><sha256 of the line>"), and a line whose hash is recorded (or that
# is exactly a line this checkout would write, for installs made before
# the record had menu lines) is Peridot's. A line that mentions Peridot
# but matches neither is one you changed: it is kept, and named.
#
# Every edit is a swap: the file is copied, the new content is composed
# from the copy, the file is checked to be still the copy, swapped for the
# new content, and the bytes that came out are checked again; if they
# aren't the copy after all, the swap is undone and nothing changed.
menu_lines() { cat dist/omarchy-menu.jsonc; }
line_hash() { printf '%s' "$1" | hash_stdin; }
declare -A MENU_KNOWN=()   # sha256 of a line -> 1, for the lines this checkout writes
load_menu_known() {
  local line
  MENU_KNOWN=()
  while IFS= read -r line || [[ -n $line ]]; do MENU_KNOWN["$(line_hash "$line")"]=1; done < <(menu_lines)
}
# menu_line_owned <line>: 0 iff the line is recorded as written to $MENU, or
# is exactly one this checkout writes.
menu_line_owned() {
  local h; h="$(line_hash "$1")"
  [[ -n ${MENU_LINE_HASH["$MENU"$'\t'"$h"]:-} || -n ${MENU_KNOWN[$h]:-} ]]
}
# Counts over the file: MENU_OWNED lines that are Peridot's, MENU_CHANGED
# lines that mention Peridot's entries but aren't.
MENU_OWNED=0 MENU_CHANGED=0
menu_count() {
  local line
  MENU_OWNED=0 MENU_CHANGED=0
  [[ -f $1 && ! -L $1 ]] || return 0
  while IFS= read -r line || [[ -n $line ]]; do
    if menu_line_owned "$line"; then MENU_OWNED=$((MENU_OWNED + 1))
    elif [[ $line == *'"trigger.share.peridot'* ]]; then MENU_CHANGED=$((MENU_CHANGED + 1)); fi
  done <"$1"
}
menu_has_entries() { menu_count "$MENU"; (( MENU_OWNED > 0 )); }
MENU_STATE=missing   # missing | other | present | edited | absent
inspect_menu() {
  (( ${#MENU_KNOWN[@]} )) || load_menu_known
  case "$(path_kind "$MENU")" in
    missing) MENU_STATE=missing ;;
    symlink | dir | other) MENU_STATE=other ;;
    file)
      menu_count "$MENU"
      if (( MENU_OWNED > 0 )); then MENU_STATE=present
      elif (( MENU_CHANGED > 0 )); then MENU_STATE=edited
      else MENU_STATE=absent; fi ;;
  esac
}
# menu_swap_in <new content file> <hash the file must still have>: puts
# the new content at $MENU only if $MENU is still exactly the bytes the new
# content was composed from, checking again after the swap. On success the
# old file is at MENU_OUT (yours to back up or remove); on failure nothing
# changed, and it says so. The new file must be in $MENU's folder.
MENU_OUT=
menu_swap_in() {
  local new=$1 expect=$2 want out dir
  dir="$(dirname -- "$MENU")"
  want="$(file_hash "$new")"
  MENU_OUT=
  if [[ -L $MENU || ! -f $MENU || "$(file_hash "$MENU")" != "$expect" ]]; then
    rm -f -- "$new"; note "$MENU changed while it was being edited; not touched. Run this again."; return 1
  fi
  if (( MV_EXCHANGE )) && mv --exchange -T -- "$new" "$MENU" 2>/dev/null; then
    out=$new
    if [[ -L $out || "$(file_hash "$out")" != "$expect" ]]; then
      # It changed between the check and the swap: put it back.
      mv --exchange -T -- "$out" "$MENU" 2>/dev/null || mv -T -- "$out" "$MENU"
      rm -f -- "$new"; note "$MENU changed while it was being edited; not touched. Run this again."; return 1
    fi
  else
    # No atomic exchange: two renames, a moment with nothing at $MENU.
    out="$(mktemp -u -- "$dir/.peridot.XXXXXX")"
    mv -T -- "$MENU" "$out" || { rm -f -- "$new"; note "couldn't move $MENU aside; not edited."; return 1; }
    if [[ "$(file_hash "$out")" != "$expect" ]]; then
      mv -T -- "$out" "$MENU"; rm -f -- "$new"; note "$MENU changed while it was being edited; not touched. Run this again."; return 1
    fi
    mv -T -- "$new" "$MENU"
  fi
  MENU_OUT=$out
  if [[ "$(file_hash "$MENU")" != "$want" ]]; then
    # Can't happen without a third party writing in the same instant; say so rather than record it.
    note "$MENU isn't what was just written to it; check it by hand ($out holds the previous content)."
    return 1
  fi
}
# Add the entries before the closing brace (the file is JSONC with
# comments, so it is edited as text), or create the file. Backs up an
# existing file first; records every line written.
menu_add_entries() {
  local tmp snap text end head h0 line dir
  dir="$(dirname -- "$MENU")"
  mkdir -p -- "$dir"
  (( ${#MENU_KNOWN[@]} )) || load_menu_known
  tmp="$(mktemp -- "$dir/.peridot.XXXXXX")"; TEMPS+=("$tmp")
  if [[ $MENU_STATE == missing ]]; then
    { echo "{"; menu_lines; echo "}"; } >"$tmp"
    chmod 644 -- "$tmp"
    ln -- "$tmp" "$MENU" 2>/dev/null || { rm -f -- "$tmp"; note "$MENU appeared meanwhile; not written."; return 1; }
    rm -f -- "$tmp"
    MANIFEST_HASH["$MENU"]="$(file_hash "$MENU")"
    while IFS= read -r line || [[ -n $line ]]; do MENU_LINE_HASH["$MENU"$'\t'"$(line_hash "$line")"]=1; done < <(menu_lines)
    write_manifest
    note "created $MENU with the Private link entries"
    return 0
  fi
  snap="$(mktemp -- "$dir/.peridot.XXXXXX")"; TEMPS+=("$snap")
  cp -p -- "$MENU" "$snap"
  h0="$(file_hash "$snap")"
  text="$(cat -- "$snap"; printf x)"; text=${text%x}
  head=${text%\}*}
  [[ $head != "$text" ]] || { rm -f -- "$tmp" "$snap"; note "$MENU has no closing brace; not edited."; return 1; }
  end=${text:${#head}}
  head="${head%"${head##*[![:space:]]}"}"
  case $head in *"{" | *",") head="$head"$'\n' ;; *) head="$head,"$'\n' ;; esac
  { printf '%s' "$head"; menu_lines; printf '%s' "$end"; } >"$tmp"
  chmod --reference="$snap" -- "$tmp" 2>/dev/null || true
  menu_swap_in "$tmp" "$h0" || { rm -f -- "$snap"; return 1; }
  rm -f -- "$snap"
  backup_file "$MENU_OUT" omarchy-menu.jsonc "$MENU"
  note "the previous $MENU is kept in $BACKUP_DEST"
  while IFS= read -r line || [[ -n $line ]]; do MENU_LINE_HASH["$MENU"$'\t'"$(line_hash "$line")"]=1; done < <(menu_lines)
  write_manifest
  note "added the Private link entries to $MENU"
}
# Take out the lines that are still exactly Peridot's; leave everything
# else, and say if some of Peridot's lines were changed and so stayed.
menu_remove_entries() {
  local tmp snap kept=0 removed=0 line h0 dir
  dir="$(dirname -- "$MENU")"
  (( ${#MENU_KNOWN[@]} )) || load_menu_known
  snap="$(mktemp -- "$dir/.peridot.XXXXXX")"; TEMPS+=("$snap")
  cp -p -- "$MENU" "$snap"
  h0="$(file_hash "$snap")"
  tmp="$(mktemp -- "$dir/.peridot.XXXXXX")"; TEMPS+=("$tmp")
  while IFS= read -r line || [[ -n $line ]]; do
    if menu_line_owned "$line"; then removed=$((removed + 1)); continue; fi
    [[ $line == *'"trigger.share.peridot'* ]] && kept=$((kept + 1))
    printf '%s\n' "$line"
  done <"$snap" >"$tmp"
  if (( removed == 0 )); then
    rm -f -- "$tmp" "$snap"
    note "$MENU: none of Peridot's lines are there unchanged; left as it is."
    (( kept )) && note "$kept Peridot menu line(s) you changed were kept; edit them out yourself if you like."
    return 0
  fi
  chmod --reference="$snap" -- "$tmp" 2>/dev/null || true
  menu_swap_in "$tmp" "$h0" || { rm -f -- "$snap"; return 1; }
  rm -f -- "$snap" "$MENU_OUT"
  note "removed $removed Private link line(s) from $MENU"
  (( kept )) && note "$kept Peridot menu line(s) you changed were kept; edit them out yourself if you like."
  forget_menu_lines
  return 0
}
# Drop the record of menu lines for $MENU.
forget_menu_lines() {
  local k
  for k in "${!MENU_LINE_HASH[@]}"; do [[ $k == "$MENU"$'\t'* ]] && unset 'MENU_LINE_HASH[$k]'; done
  write_manifest
}

# ── Release binaries ───────────────────────────────────────────────────
# The tarball a no-Rust install downloads is checked against a hash pinned
# in this checkout (dist/release-checksums.tsv), never against a checksum
# file fetched from the same release: a release can be edited, a reviewed
# commit can't. Each pin also names the source commit the build attestation
# vouched for when the pin was made.
# install.sh --dev may point this at another table (PERIDOT_RELEASE_CHECKSUMS);
# without --dev the variable is ignored and said so.
RELEASE_CHECKSUMS="dist/release-checksums.tsv"
# The minisign public key that signs dist/release-checksums.tsv, when one
# exists (install.sh sets it). Empty means "no key yet": the signature file,
# if any, isn't checked, and that is printed.
PIN_SIGNING_PUBKEY="${PIN_SIGNING_PUBKEY:-}"
# check_pin_signature: verifies $RELEASE_CHECKSUMS against
# $RELEASE_CHECKSUMS.minisig with minisign before the table is trusted.
# Dies on a bad signature. Says why when it can't check (no key in this
# checkout, no signature file, minisign not installed) and lets the
# checksum-pinning stand on its own, as before.
check_pin_signature() {
  local sig="$RELEASE_CHECKSUMS.minisig" out
  if [[ ! $PIN_SIGNING_PUBKEY =~ ^RW[A-Za-z0-9+/]{54}$ ]]; then
    [[ -e $sig ]] && note "$sig is present, but this checkout carries no signing key yet; the signature isn't checked."
    return 0
  fi
  if [[ ! -f $sig || -L $sig ]]; then
    note "$RELEASE_CHECKSUMS isn't signed ($sig is missing); relying on the pinned checksums alone."
    return 0
  fi
  if ! command -v minisign >/dev/null; then
    note "minisign isn't installed, so the signature on $RELEASE_CHECKSUMS isn't checked (pacman -S minisign)."
    return 0
  fi
  if out="$(minisign -V -q -P "$PIN_SIGNING_PUBKEY" -m "$RELEASE_CHECKSUMS" -x "$sig" 2>&1)"; then
    note "Signature on $RELEASE_CHECKSUMS verified (minisign)."
    return 0
  fi
  die "The signature on $RELEASE_CHECKSUMS doesn't verify${out:+: $out}. Not installing a download it vouches for."
}
# pinned_release <asset>: prints "<sha256> <source commit> <size>" or nothing.
pinned_release() {
  local h a c z
  [[ -f $RELEASE_CHECKSUMS ]] || return 0
  while IFS=$'\t' read -r h a c z || [[ -n $h ]]; do
    [[ $h == \#* || -z $a ]] && continue
    [[ $a == "$1" && $h =~ ^[0-9a-f]{64}$ && $z =~ ^[0-9]+$ ]] && { printf '%s %s %s\n' "$h" "$c" "$z"; return 0; }
  done <"$RELEASE_CHECKSUMS"
  # Nothing matched: say so with an empty result, not with the loop's
  # status (the last failed comparison), which under set -e would end the
  # caller silently before it can explain.
  return 0
}
# verify_pinned <file> <asset>: 0 if the file is exactly the pinned bytes
# (size first, so an oversized file is never even hashed).
verify_pinned() {
  local pin h c z
  pin="$(pinned_release "$2")"
  if [[ -z $pin ]]; then
    note "no pinned checksum for $2 in $RELEASE_CHECKSUMS."
    return 1
  fi
  read -r h c z <<<"$pin"
  if [[ "$(stat -c %s -- "$1" 2>/dev/null || echo -1)" != "$z" ]]; then
    note "$2 isn't the pinned size ($z bytes)."
    return 1
  fi
  if [[ "$(file_hash "$1")" != "$h" ]]; then
    note "$2 doesn't match the checksum pinned in this checkout."
    return 1
  fi
  PINNED_COMMIT=$c
}
PINNED_COMMIT=

print_paths() {
  say "Paths:"
  note "binaries  $BINDIR/peridotd, $BINDIR/peridot, $INSTALLER"
  note "services  $UNIT, $PROXY_UNIT (its bus)"
  note "installs  $SOCKET_UNIT, $INSTALL_UNIT (one per request; runs $INSTALLER)"
  note "plugin    $PLUGIN_PATH"
  note "menu      $MENU"
  note "record    $MANIFEST$( [[ -f $MANIFEST ]] || printf ' (none yet)')"
  note "data      $DATADIR, $CONFIGDIR, $CACHEDIR"
}
