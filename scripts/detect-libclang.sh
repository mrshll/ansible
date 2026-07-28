#!/usr/bin/env bash
#
# Print the directory bindgen should load libclang from, or exit 1 if this
# machine has none that works.
#
# A directory only qualifies when it holds libclang *and* Clang's builtin
# headers (clang/<version>/include). Both halves matter: glibc's <limits.h>
# ends in `#include_next <limits.h>`, which only resolves to Clang's own copy
# in the resource directory next to the library that got loaded. Ubuntu ships
# libclang1-N runtime packages without those headers, so a machine can have
# three libclang directories where only one can actually parse a C header.
#
# Usage: LIBCLANG_PATH="$(scripts/detect-libclang.sh)"
set -uo pipefail

# Newest first, so llvm-18 wins over an llvm-14 left behind by another package.
# Unmatched globs stay literal and fail the -e test below, which is harmless.
for dir in $(printf '%s\n' /usr/lib/llvm-*/lib | sort -rV) /usr/lib/*/; do
  [[ -e "$dir/libclang.so" || -e "$dir/libclang.so.1" ]] || continue
  for include in "$dir"/clang/*/include; do
    if [[ -f "$include/limits.h" && -f "$include/stddef.h" ]]; then
      printf '%s\n' "$dir"
      exit 0
    fi
  done
done

exit 1
