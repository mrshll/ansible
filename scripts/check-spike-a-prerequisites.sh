#!/usr/bin/env bash
set -uo pipefail

missingRequired=0

checkCommand() {
  local commandName="$1"
  if command -v "$commandName" >/dev/null 2>&1; then
    printf 'PASS command %-12s %s\n' "$commandName" "$(command -v "$commandName")"
  else
    printf 'FAIL command %-12s missing\n' "$commandName"
    missingRequired=1
  fi
}

checkPackage() {
  local packageName="$1"
  if pkg-config --exists "$packageName"; then
    printf 'PASS package %-12s %s\n' "$packageName" "$(pkg-config --modversion "$packageName")"
  else
    printf 'INFO package %-12s unavailable\n' "$packageName"
  fi
}

checkLibrary() {
  local libraryPattern="$1"
  if ldconfig -p 2>/dev/null | awk '{print $1}' | grep -Eq "$libraryPattern"; then
    printf 'PASS library %-12s installed\n' "$libraryPattern"
  else
    printf 'INFO library %-12s unavailable\n' "$libraryPattern"
  fi
}

checkCommand cargo
checkCommand rustc
checkCommand node
checkCommand npm
checkCommand pkg-config

checkPackage gtk4
checkPackage webkit2gtk-4.1
checkLibrary 'libghostty(\.so)?'

if [[ "$missingRequired" -ne 0 ]]; then
  exit 1
fi
