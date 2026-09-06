#!/bin/sh
# Regenerate (or check) the third-party license file that gets embedded
# into the binary (see build.rs and the About dialog in src/ui.rs).
#
#   sh licenses.sh          regenerate licenses.txt with cargo-about
#   sh licenses.sh --check  exit non-zero if licenses.txt is stale
#
# Stale means: any input (the resolved dependency set or the cargo-about
# config/template) is newer than the output. build.rs runs the --check
# mode before compiling and regenerates on its own when it fails; in CI
# (or a pre-commit hook) run --check directly to reject a stale file.

set -eu
cd "$(dirname "$0")"

INPUTS="Cargo.lock about.toml about-text.hbs"

if [ "${1:-}" = "--check" ]; then
    if [ ! -f licenses.txt ]; then
        echo "stale: licenses.txt is missing (run 'sh licenses.sh')" >&2
        exit 1
    fi
    for input in $INPUTS; do
        if [ "$input" -nt licenses.txt ]; then
            echo "stale: $input is newer than licenses.txt (run 'sh licenses.sh')" >&2
            exit 1
        fi
    done
    exit 0
fi

if ! command -v cargo-about >/dev/null 2>&1; then
    echo "error: cargo-about not found (install it with 'cargo install cargo-about')" >&2
    exit 1
fi

# --locked keeps the generation from touching Cargo.lock itself.
cargo about generate about-text.hbs -o licenses.txt --locked
echo "licenses.txt regenerated"
