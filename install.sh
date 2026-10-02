#!/bin/sh
set -eu

usage() {
    echo 'Usage: ./install.sh [--prefix DIRECTORY]'
    echo 'Default: $HOME/.local/bin/codex-appserver-ctl'
}

prefix="$HOME/.local"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix)
            [ "$#" -ge 2 ] && [ -n "$2" ] || { usage >&2; exit 2; }
            prefix=$2
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

command -v python3 >/dev/null 2>&1 || { echo 'python3 is required' >&2; exit 1; }
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
source_file="$root/bin/codex-appserver-ctl"
destination="$prefix/bin/codex-appserver-ctl"
python3 -c 'import sys; from pathlib import Path; p=Path(sys.argv[1]); compile(p.read_text(), str(p), "exec")' "$source_file"
mkdir -p "$prefix/bin"
if [ -L "$destination" ] || { [ -e "$destination" ] && [ ! -f "$destination" ]; }; then
    echo "Refusing to replace a symlink or non-regular file: $destination" >&2
    exit 1
fi
if [ -f "$destination" ] && cmp -s "$source_file" "$destination"; then
    chmod 755 "$destination"
    echo "Already installed: $destination"
    exit 0
fi
temporary=$(mktemp "$prefix/bin/.codex-appserver-ctl.XXXXXX")
trap 'rm -f "$temporary"' EXIT HUP INT TERM
cp "$source_file" "$temporary"
chmod 755 "$temporary"
if [ -f "$destination" ]; then
    backup=$(mktemp "$prefix/bin/codex-appserver-ctl.backup.XXXXXX")
    cp -p "$destination" "$backup"
    echo "Previous version: $backup"
fi
mv -f "$temporary" "$destination"
echo "Installed: $destination"
echo "Add $prefix/bin to PATH if needed."
