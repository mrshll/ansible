#!/usr/bin/env bash
#
# Seed the Zig package cache for a build whose dependencies Zig cannot fetch
# itself.
#
# Zig's built-in HTTP client does not negotiate every CONNECT proxy, and some
# egress policies allow `git clone` from a host while blocking its archive
# endpoints. curl and git do work in those environments, so this downloads each
# missing dependency with them and hands the artifact to `zig fetch`, which
# still verifies the content hash recorded in build.zig.zon. No hash is
# bypassed; only the transport changes.
#
# Usage: seed-zig-cache.sh <src-dir> <zig-binary> [zig build args...]
set -uo pipefail

SRC="${1:?src dir required}"; shift
ZIG="${1:?zig binary required}"; shift
BUILD_ARGS=("$@")

WORK="$(cd "$SRC" && pwd)/.zig-dep-seed"
mkdir -p "$WORK"
cd "$SRC" || exit 1

# Fetch a plain tarball URL with curl.
seed_tarball() {
  local url="$1"
  local out="$WORK/$(printf '%s' "$url" | md5sum | cut -c1-12)-$(basename "${url%%\?*}")"
  [[ -s "$out" ]] || curl -fsSL --retry 3 --max-time 900 -o "$out" "$url" || return 1
  # An HTML or JSON body means a proxy/policy refusal, not an archive.
  case "$(file -b --mime-type "$out")" in application/json|text/*) return 1 ;; esac
  "$ZIG" fetch "$out" >/dev/null 2>&1
}

# Rebuild a GitHub codeload archive locally from a git clone. Produces the same
# tree GitHub would serve, so Zig computes the same package hash.
seed_github_archive() {
  local url="$1"
  [[ "$url" =~ github\.com/([^/]+)/([^/]+)/archive/([0-9a-f]{40})\.tar\.gz ]] || return 1
  local owner="${BASH_REMATCH[1]}" repo="${BASH_REMATCH[2]}" rev="${BASH_REMATCH[3]}"
  local dir="$WORK/$repo-$rev"
  if [[ ! -d "$dir" ]]; then
    git init -q "$dir" && git -C "$dir" remote add origin "https://github.com/$owner/$repo" \
      && git -C "$dir" fetch -q --depth 1 origin "$rev" || return 1
  fi
  git -C "$dir" archive --format=tar.gz --prefix="$repo-$rev/" FETCH_HEAD -o "$dir.tar.gz" || return 1
  "$ZIG" fetch "$dir.tar.gz" >/dev/null 2>&1
}

# git+https://host/owner/repo#rev
seed_git() {
  local spec="${1#git+}" rev url repo dir
  rev="${spec##*#}"; url="${spec%%#*}"; repo="$(basename "$url" .git)"
  dir="$WORK/$repo-$rev"
  if [[ ! -d "$dir" ]]; then
    git init -q "$dir" && git -C "$dir" remote add origin "$url" \
      && git -C "$dir" fetch -q --depth 1 origin "$rev" || return 1
  fi
  git -C "$dir" archive --format=tar.gz --prefix="$repo/" FETCH_HEAD -o "$dir.tar.gz" || return 1
  "$ZIG" fetch "$dir.tar.gz" >/dev/null 2>&1
}

for round in $(seq 1 25); do
  out="$("$ZIG" build "${BUILD_ARGS[@]}" 2>&1)"
  if [[ $? -eq 0 ]]; then
    echo "    dependency seeding complete (${round} round(s))"
    exit 0
  fi

  mapfile -t urls < <(grep -oP '^\s+\.url = "\K[^"]+' <<<"$out" | sort -u)
  if [[ ${#urls[@]} -eq 0 ]]; then
    echo "$out" >&2
    exit 1
  fi

  progress=0
  for url in "${urls[@]}"; do
    if [[ "$url" == git+* ]]; then
      seed_git "$url" && { progress=1; continue; }
    elif [[ "$url" == *github.com/*/archive/* ]]; then
      seed_github_archive "$url" && { progress=1; continue; }
    else
      seed_tarball "$url" && { progress=1; continue; }
      seed_github_archive "$url" && { progress=1; continue; }
    fi
    echo "    could not seed: $url" >&2
  done
  [[ $progress -eq 0 ]] && { echo "no progress seeding dependencies" >&2; exit 1; }
done

echo "exceeded dependency seeding rounds" >&2
exit 1
