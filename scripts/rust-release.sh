#!/usr/bin/env bash
# Build a release archive of the Rust `ultron` binary. No Node.js is involved: the binary embeds its runtime files
# (crates/cli/build.rs) and needs only Python 3 at run time, for the model's REPL. Publishes nothing.
#
#   scripts/rust-release.sh [--out DIR] [--target TRIPLE]
#
# Writes DIR/ultron-<version>-<target>.tar.gz (the stripped binary, install.sh, README.md, LICENSE, NOTICE,
# THIRD_PARTY_LICENSES.txt, licenses/) and DIR/ultron-<version>-<target>.tar.gz.sha256. DIR defaults to
# dist/rust-release. Cargo runs through ../tools/cargo.sh when that exists (the rewrite's machine-load guard),
# else plain cargo. Build paths are remapped and archive metadata fixed (SOURCE_DATE_EPOCH, default: the last
# commit's time, else 0), so the same sources and toolchain give the same archive.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/dist/rust-release"
target=""
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out="$2"; shift 2 ;;
        --out=*) out="${1#--out=}"; shift ;;
        --target) target="$2"; shift 2 ;;
        --target=*) target="${1#--target=}"; shift ;;
        -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
mkdir -p "$out"
out="$(cd "$out" && pwd)"

cargo=(cargo)
if [ -x "$root/../tools/cargo.sh" ]; then
    cargo=("$root/../tools/cargo.sh")
fi
host="$(rustc -vV | sed -n 's/^host: //p')"
target="${target:-$host}"

if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
    SOURCE_DATE_EPOCH="$(git -C "$root" log -1 --format=%ct 2>/dev/null || echo 0)"
fi
export SOURCE_DATE_EPOCH

# Reproducible and free of build-machine paths: remap the checkout, cargo's registry and the home directory
# (later prefixes win in rustc, so the most specific come last). Encoded flags keep paths with spaces intact.
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
flags=(
    "--remap-path-prefix=$HOME=/home"
    "--remap-path-prefix=$cargo_home=/cargo"
    "--remap-path-prefix=$root=/ultron"
)
CARGO_ENCODED_RUSTFLAGS="$(IFS=$'\x1f'; echo "${flags[*]}")"
export CARGO_ENCODED_RUSTFLAGS
# The same for C built by the `cc` crate (tree-sitter and SQLite asserts embed __FILE__); a fresh target dir
# otherwise ships cargo's registry path. Later maps win in GCC and Clang too.
c_maps="-ffile-prefix-map=$HOME=/home -ffile-prefix-map=$cargo_home=/cargo -ffile-prefix-map=$root=/ultron"
export CFLAGS="${CFLAGS:+$CFLAGS }$c_maps"
export CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }$c_maps"
export CARGO_PROFILE_RELEASE_STRIP=symbols
export CARGO_PROFILE_RELEASE_DEBUG=0

(cd "$root" && "${cargo[@]}" build --release --locked -p cli --bin ultron --target "$target")
binary="${CARGO_TARGET_DIR:-$root/target}/$target/release/ultron"

for path in "$root" "$HOME"; do
    if grep -qaF "$path" "$binary"; then
        echo "release binary contains the build path $path" >&2
        exit 1
    fi
done

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

# The version the binary reports, from private temp dirs; `--list-models` reads the package files, so it installs
# them there (`--version` installs nothing).
probe() {
    env -i PATH="/usr/bin:/bin" HOME="$scratch/home" ULTRON_CODING_AGENT_DIR="$scratch/agent" \
        ULTRON_SERVER_DIR="$scratch/server" "$@"
}
mkdir -p "$scratch/home"
version="$(probe "$binary" --version)"
probe "$binary" --list-models > /dev/null
installed=("$scratch"/agent/runtime/"$version"-*/src/ultron/rlm/runtime.py)
[ -f "${installed[0]}" ] || { echo "the release binary did not install its runtime files" >&2; exit 1; }

name="ultron-$version-$target"
stage="$scratch/stage/$name"
mkdir -p "$stage/licenses"
cp "$binary" "$stage/ultron"
cp "$root/scripts/rust-release/install.sh" "$stage/install.sh"
cp "$root/scripts/rust-release/README.md" "$root/scripts/rust-release/NOTICE" "$root/LICENSE" "$stage/"
cp "$root/packages/coding-agent/src/ultron/loki-engine/LICENSE" "$stage/licenses/loki-engine.LICENSE"
for skill in "$root"/packages/coding-agent/skills/*/LICENSE; do
    cp "$skill" "$stage/licenses/skills-$(basename "$(dirname "$skill")").LICENSE"
done
(cd "$root" && "${cargo[@]}" metadata --locked --format-version 1 --filter-platform "$target") \
    | python3 -I "$root/scripts/rust-release/third_party_licenses.py" > "$stage/THIRD_PARTY_LICENSES.txt"
chmod 0755 "$stage/ultron" "$stage/install.sh"
find "$stage" -type f ! -name ultron ! -name install.sh -exec chmod 0644 {} +

# The staged binary runs on its own, outside the checkout.
[ "$(probe "$stage/ultron" --version)" = "$version" ]

archive="$name.tar.gz"
tar --sort=name --format=gnu --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
    -C "$scratch/stage" -cf - "$name" | gzip -n -9 > "$out/$archive"
(cd "$out" && sha256sum "$archive" > "$archive.sha256")

echo "$out/$archive"
cat "$out/$archive.sha256"
