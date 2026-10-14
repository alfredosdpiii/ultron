#!/bin/sh
# Install ultron from an unpacked release: the binary into <prefix>/bin and the license files into
# <prefix>/share/doc/ultron. No Node.js; the model's REPL needs Python 3 (ULTRON_PYTHON, else python3).
#
#   ./install.sh                 # prefix ~/.local
#   ./install.sh --prefix DIR    # or PREFIX=DIR ./install.sh
set -eu

prefix="${PREFIX:-${HOME:?HOME is not set}/.local}"
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) [ $# -ge 2 ] || { echo "--prefix needs a directory" >&2; exit 2; }; prefix="$2"; shift 2 ;;
        --prefix=*) prefix="${1#--prefix=}"; shift ;;
        -h|--help) sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

here="$(cd "$(dirname "$0")" && pwd)"
[ -f "$here/ultron" ] || { echo "no ultron binary beside $0" >&2; exit 1; }

bin="$prefix/bin"
doc="$prefix/share/doc/ultron"
mkdir -p "$bin" "$doc"
# Copy under a temporary name, then rename: a running ultron keeps its old file and nothing sees a partial copy.
cp "$here/ultron" "$bin/.ultron.install.$$"
chmod 0755 "$bin/.ultron.install.$$"
mv -f "$bin/.ultron.install.$$" "$bin/ultron"
for file in LICENSE NOTICE THIRD_PARTY_LICENSES.txt README.md; do
    if [ -f "$here/$file" ]; then cp "$here/$file" "$doc/$file"; fi
done
if [ -d "$here/licenses" ]; then
    mkdir -p "$doc/licenses"
    cp "$here"/licenses/* "$doc/licenses/"
fi

echo "Installed $("$bin/ultron" --version 2>/dev/null || echo ultron) to $bin/ultron"
python="${ULTRON_PYTHON:-python3}"
if ! command -v "$python" >/dev/null 2>&1; then
    echo "warning: $python not found; ultron's REPL needs Python 3 (set ULTRON_PYTHON to its path)" >&2
fi
case ":${PATH:-}:" in
    *":$bin:"*) ;;
    *) echo "Add $bin to your PATH to run ultron." ;;
esac
