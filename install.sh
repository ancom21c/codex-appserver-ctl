#!/bin/sh
set -eu
usage() {
    echo 'Usage: ./install.sh [--prefix DIRECTORY] [--version VERSION | --source | --binary FILE]'
    echo 'Default: download the latest release to $HOME/.local/bin/codex-appserver-ctl'
}
prefix="$HOME/.local"
binary=''
version=latest
mode=release
selected=0
work=''
temporary=''
cleanup() {
    if [ -n "$temporary" ]; then rm -f "$temporary"; fi
    if [ -n "$work" ]; then rm -rf "$work"; fi
}
trap cleanup EXIT HUP INT TERM
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix)
            [ "$#" -ge 2 ] && [ -n "$2" ] || { usage >&2; exit 2; }
            prefix=$2; shift 2 ;;
        --binary|--version)
            [ "$#" -ge 2 ] && [ -n "$2" ] && [ "$selected" = 0 ] || { usage >&2; exit 2; }
            selected=1
            if [ "$1" = --binary ]; then binary=$2; mode=binary; else version=$2; fi
            shift 2 ;;
        --source)
            [ "$selected" = 0 ] || { usage >&2; exit 2; }
            selected=1; mode=source; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done
if [ "$mode" = source ]; then
    root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
    [ -f "$root/Cargo.toml" ] || { echo '--source requires a repository checkout' >&2; exit 1; }
    command -v cargo >/dev/null 2>&1 || { echo 'Cargo, Rust 1.85+, and a C linker are required for --source.' >&2; exit 1; }
    cargo build --manifest-path "$root/Cargo.toml" --release --locked
    binary="$root/target/release/codex-appserver-ctl"
elif [ "$mode" = release ]; then
    case "$version" in
        latest) release_path=latest/download ;;
        *)
            version=${version#v}
            case "$version" in ''|*[!0-9A-Za-z.+-]*) echo 'Invalid release version' >&2; exit 2 ;; esac
            release_path="download/v$version" ;;
    esac
    os=$(uname -s)
    cpu=$(uname -m)
    case "$cpu" in arm64|aarch64) arch=aarch64 ;; x86_64|amd64) arch=x86_64 ;; *) echo "Unsupported CPU: $cpu. Use --source." >&2; exit 1 ;; esac
    case "$os" in Darwin) target="$arch-apple-darwin" ;; Linux) target="$arch-unknown-linux-musl" ;; *) echo "Unsupported OS: $os" >&2; exit 1 ;; esac
    command -v curl >/dev/null 2>&1 || { echo 'curl is required' >&2; exit 1; }
    command -v tar >/dev/null 2>&1 || { echo 'tar is required' >&2; exit 1; }
    if command -v sha256sum >/dev/null 2>&1; then checksum=sha256sum
    elif command -v shasum >/dev/null 2>&1; then checksum=shasum
    else echo 'sha256sum or shasum is required' >&2; exit 1; fi
    work=$(mktemp -d)
    asset="codex-appserver-ctl-$target.tar.gz"
    url="https://github.com/ancom21c/codex-appserver-ctl/releases/$release_path"
    for file in "$asset" "$asset.sha256"; do
        curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
            --connect-timeout 10 --max-time 120 --output "$work/$file" "$url/$file" || {
            echo "Release download failed: $file. Use --source from a checkout to build locally." >&2; exit 1;
        }
    done
    # Validate the digest and calculate it ourselves. Do not accept paths from the checksum file.
    expected=$(awk 'NR == 1 {print $1}' "$work/$asset.sha256")
    case "$expected" in ''|*[!0-9a-fA-F]*) echo 'Invalid SHA-256 file' >&2; exit 1 ;; esac
    [ "${#expected}" = 64 ] || { echo 'Invalid SHA-256 length' >&2; exit 1; }
    if [ "$checksum" = sha256sum ]; then actual=$(sha256sum "$work/$asset" | awk '{print $1}')
    else actual=$(shasum -a 256 "$work/$asset" | awk '{print $1}'); fi
    [ "$actual" = "$expected" ] || { echo 'SHA-256 mismatch; installation stopped' >&2; exit 1; }
    [ "$(tar -tzf "$work/$asset")" = codex-appserver-ctl ] || { echo 'Invalid release archive' >&2; exit 1; }
    tar -xzf "$work/$asset" -C "$work"
    binary="$work/codex-appserver-ctl"
fi
[ -f "$binary" ] && [ ! -L "$binary" ] && [ -x "$binary" ] || { echo "Invalid executable: $binary" >&2; exit 1; }
destination="$prefix/bin/codex-appserver-ctl"
mkdir -p "$prefix/bin"
if [ -L "$destination" ] || { [ -e "$destination" ] && [ ! -f "$destination" ]; }; then
    echo "Refusing to replace a symlink or non-regular file: $destination" >&2; exit 1
fi
if [ -f "$destination" ] && [ -z "$(find "$destination" -user "$(id -un)" -print)" ]; then
    echo "Refusing to replace a file owned by another user: $destination" >&2; exit 1
fi
if [ -f "$destination" ] && cmp -s "$binary" "$destination"; then
    echo "Already installed: $destination"; exit 0
fi
temporary=$(mktemp "$prefix/bin/.codex-appserver-ctl.XXXXXX")
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
