#!/usr/bin/env bash
source "$T/lib.sh"

# A copy as 0.1.x left it: this checkout's files plus the URL-only marker.
v01_copy() { mkdir -p "$P"; cp "$ROOT"/shell-plugin/* "$P/"; rm -f "$P/manifest.json"; (cd "$ROOT" && jq '.entryPoints |= with_entries(.value |= ltrimstr("shell-plugin/"))' manifest.json) >"$P/manifest.json"; printf 'https://github.com/derekross/peridot\n' >"$P/.installed-by-peridot"; }

case_ "1. a 0.1.x copy (URL marker, no record): replaced, marker removed, your extra file kept"
fresh; v01_copy; echo x >"$P/extra.txt"; r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "marker gone" [ ! -e "$P/.installed-by-peridot" ] && expect "extra kept" [ -f "$P/extra.txt" ] &&
expect "only extra named" [ "$(grep -c keeping "$HOME/out.txt")" = 1 ] && expect "manifest verifies" manifest_verifies && ok

case_ "2. a marker with hash lines (an unreleased build) counts as a record; a marker you edited is yours"
fresh; v01_copy; (cd "$P" && { printf 'https://github.com/derekross/peridot\n'; sha256sum PeridotService.qml Widget.qml; } >.installed-by-peridot); echo "// edited" >>"$P/Widget.qml"; cp "$P/Widget.qml" "$HOME/w"
r=$(inst); expect "exit 0" [ "$r" = 0 ] && expect "edited kept" same "$HOME/w" "$P/Widget.qml" && expect "marker gone" [ ! -e "$P/.installed-by-peridot" ] && ok
fresh; v01_copy; echo "my notes" >>"$P/.installed-by-peridot"; r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "edited marker kept" grep -q "my notes" "$P/.installed-by-peridot" && said "keeping .installed-by-peridot" && ok

case_ "3. v0.1.0 files (no marker at all) are recognised by their release hashes and replaced"
fresh; mkdir -p "$P"; cp "$T"/fixtures/plugin-v0.1.0/* "$P/"; r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "replaced" same "$ROOT/shell-plugin/Widget.qml" "$P/Widget.qml" && same "$ROOT/shell-plugin/PeridotService.qml" "$P/PeridotService.qml" &&
expect "manifest replaced" [ "$(jq -r .version "$P/manifest.json")" = "$(jq -r .version "$ROOT/manifest.json")" ] && expect "nothing kept" not said "keeping" && ok

case_ "5. a nested .installed-by-peridot is an ordinary file of yours"
fresh; inst >/dev/null; mkdir "$P/sub"; printf 'https://github.com/derekross/peridot\n' >"$P/sub/.installed-by-peridot"; r=$(inst)
expect "kept" [ -f "$P/sub/.installed-by-peridot" ] && ok

case_ "6. a record line pointing outside Peridot's paths is ignored by uninstall"
fresh; inst >/dev/null; echo secret >"$HOME/.bashrc"; h=$(sha256sum "$HOME/.bashrc" | cut -c1-64); printf '%s\t%s\n' "$h" "$HOME/.bashrc" >>"$M"; r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "bashrc intact" [ "$(cat "$HOME/.bashrc")" = secret ] && said "ignoring a record line for $HOME/.bashrc" && ok

finish
