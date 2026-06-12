#!/usr/bin/env sh
set -eu

compiler="${CC:-cc}"
configuration="release"
nuked_root="src/nuked-opl3"
output_name="c_bench_full_file"

usage() {
    cat >&2 <<EOF
usage: $0 [--compiler CC] [--configuration NAME] [--nuked-root DIR] [--output-name NAME]

Builds tools/c_bench_full_file.c against the selected Nuked-OPL3 source tree.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h)
            usage
            exit 0
            ;;
        --compiler|-Compiler)
            shift
            [ "$#" -gt 0 ] || { echo "error: --compiler requires a value" >&2; exit 1; }
            compiler="$1"
            ;;
        --configuration|-Configuration)
            shift
            [ "$#" -gt 0 ] || { echo "error: --configuration requires a value" >&2; exit 1; }
            configuration="$1"
            ;;
        --nuked-root|-NukedRoot)
            shift
            [ "$#" -gt 0 ] || { echo "error: --nuked-root requires a value" >&2; exit 1; }
            nuked_root="$1"
            ;;
        --output-name|-OutputName)
            shift
            [ "$#" -gt 0 ] || { echo "error: --output-name requires a value" >&2; exit 1; }
            output_name="$1"
            ;;
        *)
            echo "error: unknown option $1" >&2
            usage
            exit 1
            ;;
    esac
    shift
done

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH= cd -- "$script_dir/.." && pwd)

case "$nuked_root" in
    /*) nuked_dir="$nuked_root" ;;
    *) nuked_dir="$root/$nuked_root" ;;
esac

[ -f "$nuked_dir/opl3.h" ] || { echo "error: opl3.h was not found under $nuked_dir" >&2; exit 1; }
[ -f "$nuked_dir/opl3.c" ] || { echo "error: opl3.c was not found under $nuked_dir" >&2; exit 1; }

out_dir="$root/target/$configuration"
out="$out_dir/$output_name"
mkdir -p "$out_dir"

"$compiler" -O2 -DNDEBUG -std=c99 -D_POSIX_C_SOURCE=199309L \
    -I "$nuked_dir" \
    "$root/tools/c_bench_full_file.c" "$nuked_dir/opl3.c" \
    -o "$out"

printf '%s\n' "$out"
