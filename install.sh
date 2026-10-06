#!/bin/sh
set -eu
usage() {
    echo 'Usage: ./install.sh [--prefix DIRECTORY] [--binary FILE]'
    echo 'Default: $HOME/.local/bin/codex-appserver-ctl'
}
prefix="$HOME/.local"
binary=''
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix|--binary)
            [ "$#" -ge 2 ] && [ -n "$2" ] || { usage >&2; exit 2; }
            if [ "$1" = --prefix ]; then prefix=$2; else binary=$2; fi
            shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ -z "$binary" ]; then
    command -v cargo >/dev/null 2>&1 || { echo 'Cargo and a C linker are required to build. Use --binary FILE to install a compiled binary.' >&2; exit 1; }
    cargo build --manifest-path "$root/Cargo.toml" --release --locked
    binary="$root/target/release/codex-appserver-ctl"
fi
[ -f "$binary" ] && [ -x "$binary" ] || { echo "Invalid executable: $binary" >&2; exit 1; }
destination="$prefix/bin/codex-appserver-ctl"
mkdir -p "$prefix/bin"
if [ -L "$destination" ] || { [ -e "$destination" ] && [ ! -f "$destination" ]; }; then
    echo "Refusing to replace a symlink or non-regular file: $destination" >&2
    exit 1
fi
if [ -f "$destination" ] && [ -z "$(find "$destination" -user "$(id -un)" -print)" ]; then
    echo "Refusing to replace a file owned by another user: $destination" >&2
    exit 1
fi
if [ -f "$destination" ] && cmp -s "$binary" "$destination"; then
    echo "Already installed: $destination"
    exit 0
fi
temporary=$(mktemp "$prefix/bin/.codex-appserver-ctl.XXXXXX")
trap 'rm -f "$temporary"' EXIT HUP INT TERM
cp "$binary" "$temporary"
chmod 755 "$temporary"
if [ -f "$destination" ]; then
    backup=$(mktemp "$prefix/bin/codex-appserver-ctl.backup.XXXXXX")
    cp -p "$destination" "$backup"
    echo "Previous version: $backup"
fi
mv -f "$temporary" "$destination"
echo "Installed: $destination"
echo "Add $prefix/bin to PATH if needed."
