#!/usr/bin/env bash
# Every file an Peridot version has put on disk, by SHA-256, so install.sh and
# uninstall.sh can tell a file Peridot wrote (any version) from one you changed.
#
#   dist/gen-known-hashes.sh            regenerate dist/known-hashes.tsv on stdout
#   dist/gen-known-hashes.sh --check    is every file of this checkout listed?
#
# Covers every commit on the branch since the shell plugin appeared (users
# install from `main`, not only tags), every tag, and the working tree. Run
# it and commit the result when bumping the version; the release workflow
# runs --check so a tag whose files aren't listed can't be released.
#
# Line format: <sha256>\t<label>\t<logical path>, tab-separated. Logical
# paths: `unit` (dist/peridot.service), `install-unit`
# (dist/peridot-install@.service), `install-socket`
# (dist/peridot-install.socket), `proxy-unit`
# (dist/peridot-dbus-proxy.service), `bin/peridot-install` (the install
# script), `plugin/<file>` (what the plugin copy holds; manifest.json as
# install.sh renders it), `plugin/.installed-by-peridot` (the marker earlier
# installs wrote). The label is the tag or version+commit
# that first shipped the content; only the hash and the path are compared.
set -euo pipefail
cd "$(dirname "$0")/.."

FIRST=7f89c78   # "Omarchy panel, installer, CI and release workflows"
TRANSFORM='.entryPoints |= with_entries(.value |= ltrimstr("shell-plugin/"))'

hash_stdin() { sha256sum | cut -d' ' -f1; }

# Lines for one git ref.
ref_lines() {
  local ref=$1 label=$2 path rel
  if git cat-file -e "$ref:dist/peridot.service" 2>/dev/null; then
    printf '%s\t%s\t%s\n' "$(git show "$ref:dist/peridot.service" | hash_stdin)" "$label" unit
  fi
  if git cat-file -e "$ref:dist/peridot-install@.service" 2>/dev/null; then
    printf '%s\t%s\t%s\n' "$(git show "$ref:dist/peridot-install@.service" | hash_stdin)" "$label" install-unit
  fi
  if git cat-file -e "$ref:dist/peridot-install" 2>/dev/null; then
    printf '%s\t%s\t%s\n' "$(git show "$ref:dist/peridot-install" | hash_stdin)" "$label" bin/peridot-install
  fi
  if git cat-file -e "$ref:dist/peridot-install.socket" 2>/dev/null; then
    printf '%s\t%s\t%s\n' "$(git show "$ref:dist/peridot-install.socket" | hash_stdin)" "$label" install-socket
  fi
  if git cat-file -e "$ref:dist/peridot-dbus-proxy.service" 2>/dev/null; then
    printf '%s\t%s\t%s\n' "$(git show "$ref:dist/peridot-dbus-proxy.service" | hash_stdin)" "$label" proxy-unit
  fi
  while IFS= read -r path; do
    rel=${path#shell-plugin/}
    [[ $rel == manifest.json ]] && continue
    printf '%s\t%s\t%s\n' "$(git show "$ref:$path" | hash_stdin)" "$label" "plugin/$rel"
  done < <(git ls-tree -r --name-only "$ref" -- shell-plugin)
  # The copy's manifest: verbatim while it lived in shell-plugin/, rendered
  # from the root manifest since then (0b78924).
  if git cat-file -e "$ref:shell-plugin/manifest.json" 2>/dev/null; then
    printf '%s\t%s\t%s\n' "$(git show "$ref:shell-plugin/manifest.json" | hash_stdin)" "$label" plugin/manifest.json
  elif git cat-file -e "$ref:manifest.json" 2>/dev/null; then
    printf '%s\t%s\t%s\n' "$(git show "$ref:manifest.json" | jq "$TRANSFORM" | hash_stdin)" "$label" plugin/manifest.json
  fi
}

# Lines for the working tree.
tree_lines() {
  local label=$1 path rel
  printf '%s\t%s\t%s\n' "$(hash_stdin <dist/peridot.service)" "$label" unit
  printf '%s\t%s\t%s\n' "$(hash_stdin <dist/peridot-install@.service)" "$label" install-unit
  printf '%s\t%s\t%s\n' "$(hash_stdin <dist/peridot-install)" "$label" bin/peridot-install
  printf '%s\t%s\t%s\n' "$(hash_stdin <dist/peridot-dbus-proxy.service)" "$label" proxy-unit
  printf '%s\t%s\t%s\n' "$(hash_stdin <dist/peridot-install.socket)" "$label" install-socket
  while IFS= read -r -d '' path; do
    rel=${path#shell-plugin/}
    [[ $rel == manifest.json ]] && continue
    printf '%s\t%s\t%s\n' "$(hash_stdin <"$path")" "$label" "plugin/$rel"
  done < <(find shell-plugin -type f -print0 | LC_ALL=C sort -z)
  printf '%s\t%s\t%s\n' "$(jq "$TRANSFORM" manifest.json | hash_stdin)" "$label" plugin/manifest.json
}

ref_label() {
  local ref=$1 tag
  tag="$(git tag --points-at "$ref" | head -1)"
  if [[ -n $tag ]]; then
    echo "$tag"
  elif git cat-file -e "$ref:manifest.json" 2>/dev/null; then
    echo "$(git show "$ref:manifest.json" | jq -r .version)+$(git rev-parse --short "$ref")"
  else
    echo "$(git show "$ref:shell-plugin/manifest.json" | jq -r .version)+$(git rev-parse --short "$ref")"
  fi
}

generate() {
  {
    for ref in $(git rev-list --reverse "$FIRST^..HEAD" -- shell-plugin manifest.json dist/peridot.service dist/peridot-install@.service dist/peridot-install dist/peridot-dbus-proxy.service dist/peridot-install.socket) \
               $(git tag -l 'v*'); do
      ref_lines "$ref" "$(ref_label "$ref")"
    done
    # The marker 0.1.x wrote into the plugin copy.
    printf '%s\t%s\t%s\n' "$(printf 'https://github.com/derekross/peridot\n' | hash_stdin)" v0.1.0 plugin/.installed-by-peridot
    tree_lines "$(jq -r .version manifest.json)"
  } | LC_ALL=C sort -t $'\t' -k3,3 -k1,1 -s | awk -F'\t' '!seen[$1 FS $3]++'
}

case "${1:-}" in
  "")
    echo "# generated by dist/gen-known-hashes.sh; do not edit. sha256, first label, logical path."
    generate
    ;;
  --check)
    [[ -f dist/known-hashes.tsv ]] || { echo "dist/known-hashes.tsv is missing" >&2; exit 1; }
    missing=0
    while IFS=$'\t' read -r h _ logical; do
      grep -qF -- "$h	" dist/known-hashes.tsv && grep -F -- "$h	" dist/known-hashes.tsv | grep -qF -- "	$logical" \
        || { echo "not listed: $logical ($h)"; missing=1; }
    done < <(tree_lines check)
    if (( missing )); then
      echo "Run dist/gen-known-hashes.sh > dist/known-hashes.tsv and commit it." >&2
      exit 1
    fi
    echo "known-hashes.tsv covers this checkout."
    ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac
