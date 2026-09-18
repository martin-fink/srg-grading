#!/bin/bash
set -eu

# Overrides let the local smoke test use temporary directories without Kubernetes.
recipe_dir=${RECIPE_DIR:-/recipe}
output_dir=${OUTPUT_DIR:-/output}
compiler=${CC:-/bin/cc}

mkdir -p "$output_dir/launcher"
"$compiler" -c "$recipe_dir/launcher.c" -o "$output_dir/launcher/launcher.o"
printf 'Prepared echo launcher cache\n'
