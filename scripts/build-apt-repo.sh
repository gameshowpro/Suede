#!/usr/bin/env bash
#
# Build Suede's signed apt repository, served from the documentation site at
# https://suede.gameshow.pro/apt/.
#
#   Usage: scripts/build-apt-repo.sh OUTDIR
#
# The packages come from GitHub Releases and are copied into OUTDIR, not
# linked. apt resolves every `Filename:` relative to the repository root and
# cannot follow an absolute URL to a release asset, and GitHub Pages cannot
# redirect a pool path to one either, so the only layout apt can install from
# is one where the .deb files sit inside the published site. Nothing here is
# committed to git: CI rebuilds the whole repository into the Pages artifact
# on every docs deploy.
#
# Only attested releases are mirrored. A release is kept when it is neither a
# draft nor a prerelease, has a package for every architecture below, and
# every one of those packages has a build provenance attestation that
# `gh attestation verify` accepts as made by this repository's
# .github/workflows/ci.yml from refs/heads/main. The repository signature only
# says CI built the index; this is what ties each package in it to a CI build
# of main, so a release asset replaced by hand after CI built it is refused
# rather than signed into every appliance's trust.
#
# The filter takes the place of a version floor. Releases from before CI
# recorded attestations have none, so the first release that does is the
# oldest one served, without a hardcoded version that would need maintaining.
# A package with no attestation at all (the API answers 404) skips its whole
# release with a warning. Anything else, a lookup that fails for another
# reason or an attestation that exists but does not verify, stops the build:
# treating a GitHub outage as "no attestation" would quietly empty the
# repository, and an attestation that fails to verify means something is
# wrong, not missing.
#
# Environment:
#   APT_SIGNING_KEY    ASCII-armored private key, no passphrase. Required.
#                      There is deliberately no unsigned mode: every appliance
#                      that trusts this repository would accept whatever it
#                      served, so without the key the script refuses to run.
#   APT_KEEP_RELEASES  How many attested releases to mirror, newest first.
#                      Default 20. Releases the filter skips do not count.
#                      Every mirrored package is two .deb files in the Pages
#                      artifact, which has a size limit; older releases stay on
#                      the GitHub releases page.
#   GH_REPO            Repository to read releases from. Default
#                      gameshowpro/Suede.
#   GH_TOKEN           Token for `gh`, as `gh` expects it.
#   SKIP_DOWNLOAD      Set to 1 to skip GitHub entirely and index whatever is
#                      already in OUTDIR/pool/main/s/suede/. For testing the
#                      repository layout and signing without a token. It
#                      bypasses the attestation filter: whatever is in the
#                      pool is indexed and signed, so never use it in CI.
#   VERIFY_ONLY        Test hook. Set to 1 and pass TAG FILE instead of OUTDIR
#                      to run only the provenance check on one file and exit:
#                      0 if it verifies, 3 if it has no attestation, and an
#                      error (1) for anything else. Needs `gh`, nothing more.
#
# The signing key is made once, by a maintainer, and kept only in the
# repository secret:
#
#   gpg --batch --quick-generate-key "Suede apt repository <suede@gameshow.pro>" ed25519 sign never
#   gpg --armor --export-secret-keys suede@gameshow.pro | gh secret set APT_SIGNING_KEY --repo gameshowpro/Suede
#
# Layout written to OUTDIR:
#   pool/main/s/suede/suede_<version>_<arch>.deb
#   dists/stable/{Release,Release.gpg,InRelease}
#   dists/stable/main/binary-{amd64,arm64}/Packages{,.gz}
#   dists/stable/main/binary-{amd64,arm64}/by-hash/SHA{256,512}/<digest>
#   suede-archive-keyring.gpg   the public key, binary, for Signed-By
#   suede-archive-keyring.asc   the same key, armored
#   index.html                  what this is and how to use it
set -euo pipefail

usage() {
  sed -n 's/^#   Usage: //p' "$0" >&2
  exit 2
}

KEEP="${APT_KEEP_RELEASES:-20}"
REPO="${GH_REPO:-gameshowpro/Suede}"
ARCHES=(amd64 arm64)
POOL="pool/main/s/suede"
# Also the most releases examined, so a long unattested history ends the walk
# instead of downloading every release ever made.
LIST_LIMIT=200

# In a workflow log, warnings become annotations rather than scrolling past.
warn() {
  if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    echo "::warning::$*"
  else
    echo "warning: $*" >&2
  fi
}
die() {
  if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    echo "::error::$*"
  else
    echo "error: $*" >&2
  fi
  exit 1
}

# verify_provenance TAG FILE
# Returns 0 when FILE's attestation verifies, and 1, with a warning, when it
# has none. Any other outcome stops the build. Written with explicit checks
# because callers use it as a condition, where `set -e` does not apply.
verify_provenance() {
  local tag="$1" file="$2" name digest err
  name=$(basename "$file")
  digest=$(sha256sum "$file" | cut -d' ' -f1)
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || die "$tag: could not hash $name"
  # `gh attestation verify` exits non-zero both when there is no attestation
  # and when it cannot reach GitHub. The REST endpoint tells the two apart: a
  # 404 means none exists for this digest in this repository.
  if ! err=$(gh api "repos/$REPO/attestations/sha256:$digest" --silent 2>&1 >/dev/null); then
    if grep -q '(HTTP 404)' <<<"$err"; then
      warn "$tag: $name has no build provenance attestation; skipping this release"
      return 1
    fi
    printf '%s\n' "$err" >&2
    die "$tag: could not look up the attestation of $name"
  fi
  # The signer workflow and the ref pin it to a CI build of main. --repo alone
  # would also accept a build from a preview branch, which never went through
  # review.
  if ! gh attestation verify "$file" --repo "$REPO" \
      --signer-workflow "$REPO/.github/workflows/ci.yml" \
      --source-ref refs/heads/main >&2; then
    die "$tag: $name has an attestation, but it does not verify as built by $REPO's ci.yml from refs/heads/main"
  fi
  return 0
}

if [[ "${VERIFY_ONLY:-}" == "1" ]]; then
  [[ $# -eq 2 ]] || die "VERIFY_ONLY=1 takes TAG FILE"
  [[ -f "$2" ]] || die "$2 is not a file"
  for tool in gh sha256sum; do
    command -v "$tool" >/dev/null || die "$tool is not installed"
  done
  # Exit 3 rather than 1 for "no attestation", so a test can tell it from
  # die's exit status.
  verify_provenance "$1" "$2" || exit 3
  echo "$1: $(basename "$2") verifies"
  exit 0
fi

[[ $# -eq 1 ]] || usage
OUTDIR="$1"

[[ "$KEEP" =~ ^[1-9][0-9]*$ ]] || die "APT_KEEP_RELEASES must be a positive integer, not '$KEEP'"
[[ -n "${APT_SIGNING_KEY:-}" ]] || die "APT_SIGNING_KEY is not set; refusing to build an unsigned repository"

required=(gpg gpgv gpgconf apt-ftparchive dpkg-deb gzip sha256sum)
[[ "${SKIP_DOWNLOAD:-}" == "1" ]] || required+=(gh)
for tool in "${required[@]}"; do
  if ! command -v "$tool" >/dev/null; then
    case "$tool" in
      apt-ftparchive) die "apt-ftparchive is not installed (it is in apt-utils)" ;;
      *) die "$tool is not installed" ;;
    esac
  fi
done

# ---------------------------------------------------------------------------
# The signing key lives in a keyring of its own for the length of this run.
# Importing into the caller's keyring would leave a private key behind on
# whatever machine ran the script, and would let some other secret key in
# that keyring sign the repository by default.
#
# Under /tmp regardless of TMPDIR: gpg-agent's socket lives in GNUPGHOME and a
# Unix socket path is limited to 108 bytes. A long TMPDIR made the agent
# refuse to start, and every gpg call after it failed with "No agent running".
# ---------------------------------------------------------------------------
GNUPGHOME="$(mktemp -d /tmp/suede-apt-gnupg.XXXXXXXX)"
export GNUPGHOME
chmod 700 "$GNUPGHOME"
cleanup() {
  # The agent is started inside GNUPGHOME and holds the key in memory; stop
  # it before removing the directory it runs from.
  gpgconf --kill gpg-agent >/dev/null 2>&1 || true
  rm -rf "$GNUPGHOME"
}
trap cleanup EXIT

echo "==> Importing the signing key"
printf '%s\n' "$APT_SIGNING_KEY" | gpg --batch --quiet --import \
  || die "APT_SIGNING_KEY could not be imported; it must be an ASCII-armored private key"
secret_keys=$(gpg --batch --with-colons --list-secret-keys | grep -c '^sec:' || true)
[[ "$secret_keys" -eq 1 ]] \
  || die "APT_SIGNING_KEY must hold exactly one private key, found $secret_keys"
FINGERPRINT=$(gpg --batch --with-colons --list-secret-keys \
  | awk -F: '/^fpr:/ { print $10; exit }')
[[ -n "$FINGERPRINT" ]] || die "could not read the signing key's fingerprint"
echo "    $FINGERPRINT"

# ---------------------------------------------------------------------------
# Fill the pool.
# ---------------------------------------------------------------------------
if [[ "${SKIP_DOWNLOAD:-}" == "1" ]]; then
  echo "==> SKIP_DOWNLOAD=1: indexing what is already in $OUTDIR/$POOL"
  [[ -d "$OUTDIR/$POOL" ]] || die "$OUTDIR/$POOL does not exist"
  # Everything else is regenerated from the pool, so nothing from an earlier
  # run can survive into this one.
  rm -rf "$OUTDIR/dists" "$OUTDIR/index.html" \
    "$OUTDIR/suede-archive-keyring.gpg" "$OUTDIR/suede-archive-keyring.asc"
else
  # A fresh directory only. If mkdocs ever put something at this path, mixing
  # the two would publish a repository nobody can reason about.
  if [[ -e "$OUTDIR" ]] && [[ -n "$(ls -A "$OUTDIR")" ]]; then
    die "$OUTDIR already exists and is not empty"
  fi
  mkdir -p "$OUTDIR/$POOL"

  echo "==> Listing releases of $REPO"
  # Drafts are excluded: they are not public, so their assets would be the
  # only copy anyone outside the project could get. Prereleases are excluded
  # too: apt would offer one as an ordinary upgrade to every appliance on the
  # stable suite. Tags are sorted by version rather than by date, so
  # re-publishing an old release cannot push a newer one out of the window.
  # They are walked newest first until $KEEP have been kept, so releases the
  # attestation filter skips do not use up the window.
  mapfile -t tags < <(
    gh release list --repo "$REPO" --limit "$LIST_LIMIT" \
      --json tagName,isDraft,isPrerelease \
      --jq '.[] | select((.isDraft or .isPrerelease) | not) | .tagName' \
      | grep -E '^v[0-9]' | sort -V -r
  )
  [[ ${#tags[@]} -gt 0 ]] || die "no releases of $REPO to mirror"
  echo "    ${#tags[@]} published releases; keeping the newest $KEEP that are attested"

  staging="$(mktemp -d)"
  trap 'cleanup; rm -rf "$staging"' EXIT
  kept=()
  for tag in "${tags[@]}"; do
    [[ ${#kept[@]} -lt $KEEP ]] || break
    rm -rf "$staging/debs"
    mkdir "$staging/debs"
    if ! gh release download "$tag" --repo "$REPO" --dir "$staging/debs" \
        --pattern 'suede_*_amd64.deb' --pattern 'suede_*_arm64.deb' 2>"$staging/err"; then
      # A release without packages (a docs-only or failed release) is skipped.
      # Anything else, a network or permissions failure, would silently drop
      # a release from the repository, so it stops the build instead.
      if grep -qi 'no assets' "$staging/err"; then
        warn "$tag has no suede packages; skipping it"
        continue
      fi
      cat "$staging/err" >&2
      die "could not download the packages of $tag"
    fi
    # A release missing an architecture would leave those machines on an
    # older version while the rest move on, with nothing telling them why.
    missing=""
    for arch in "${ARCHES[@]}"; do
      compgen -G "$staging/debs/suede_*_${arch}.deb" >/dev/null || missing+=" $arch"
    done
    if [[ -n "$missing" ]]; then
      warn "$tag has no package for${missing}; skipping it"
      continue
    fi
    # Every file is checked before any of them reaches the pool, so a release
    # is mirrored whole or not at all.
    attested=1
    for deb in "$staging/debs"/*.deb; do
      if ! verify_provenance "$tag" "$deb"; then
        attested=0
        break
      fi
    done
    [[ "$attested" -eq 1 ]] || continue
    for deb in "$staging/debs"/*.deb; do
      [[ -e "$deb" ]] || continue
      mv "$deb" "$OUTDIR/$POOL/"
    done
    kept+=("$tag")
    echo "    $tag"
  done
  echo "    kept ${#kept[@]} of at most $KEEP"
fi

# ---------------------------------------------------------------------------
# Check every file before it is indexed. apt-ftparchive picks packages for an
# architecture by filename alone (*_amd64.deb, *_all.deb), so a file whose
# name and control data disagree would be offered to machines it cannot run
# on. Anything that is not a Suede package for a supported architecture stops
# the build rather than being dropped quietly.
# ---------------------------------------------------------------------------
echo "==> Checking the pool"
shopt -s nullglob
debs=("$OUTDIR/$POOL"/*.deb)
shopt -u nullglob
[[ ${#debs[@]} -gt 0 ]] || die "the pool is empty; refusing to publish a repository with nothing in it"
stray=$(find "$OUTDIR/pool" -type f ! -path "$OUTDIR/$POOL/*.deb" -print)
[[ -z "$stray" ]] || die "unexpected files in the pool: $stray"

declare -A versions=() seen=()
for deb in "${debs[@]}"; do
  name=$(basename "$deb")
  package=$(dpkg-deb -f "$deb" Package 2>/dev/null) || die "$name is not a readable .deb"
  [[ "$package" == "suede" ]] || die "$name is package '$package', not suede"
  arch=$(dpkg-deb -f "$deb" Architecture)
  case " ${ARCHES[*]} " in
    *" $arch "*) ;;
    *) die "$name is for architecture '$arch', which this repository does not serve" ;;
  esac
  [[ "$name" == *"_${arch}.deb" ]] || die "$name says Architecture: $arch, but its name does not end in _${arch}.deb"
  version=$(dpkg-deb -f "$deb" Version)
  # Two files with one version and architecture give apt two different
  # packages under the same name, and which it installs depends on the order
  # it read them in.
  [[ -z "${seen["$version $arch"]:-}" ]] \
    || die "$name and ${seen["$version $arch"]} are both suede $version for $arch"
  seen["$version $arch"]="$name"
  versions["$version"]=1
  echo "    $name  $version  $arch"
done

# ---------------------------------------------------------------------------
# Index. Run from OUTDIR so every `Filename:` is pool/..., relative to the
# repository root, which is the only form apt accepts.
# ---------------------------------------------------------------------------
echo "==> Writing the package indexes"
for arch in "${ARCHES[@]}"; do
  dir="dists/stable/main/binary-$arch"
  mkdir -p "$OUTDIR/$dir"
  (cd "$OUTDIR" && apt-ftparchive --arch "$arch" packages pool > "$dir/Packages")
  gzip -9 -k -n -f "$OUTDIR/$dir/Packages"
  count=$(grep -c '^Package: ' "$OUTDIR/$dir/Packages" || true)
  [[ "$count" -gt 0 ]] || warn "no $arch packages; $arch machines will find nothing to install"
  echo "    $arch: $count"
done

# Origin and Label are what unattended-upgrades matches against. Neither is a
# distribution's name, so a stock Allowed-Origins never matches and Suede is
# never upgraded behind an operator's back (an upgrade restarts the daemon and
# blanks the displays). They are also what an operator writes if they ever do
# want it, so they must not change once published.
echo "==> Writing the Release file"
# Written outside the directory it describes, or apt-ftparchive hashes its
# own half-written output.
#
# DoByHash: GitHub Pages serves through a CDN, so for a while after a deploy a
# client can fetch the new InRelease and a cached old Packages, and fail on a
# hash mismatch. With by-hash, apt fetches each index by its own digest
# (dists/stable/main/binary-*/by-hash/SHA256/<digest>), a path that never
# changes meaning, so a stale cache can only miss, never mislead.
release_tmp="$(mktemp)"
apt-ftparchive \
  -o APT::FTPArchive::DoByHash=true \
  -o APT::FTPArchive::Release::Origin=Suede \
  -o APT::FTPArchive::Release::Label=Suede \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o "APT::FTPArchive::Release::Architectures=${ARCHES[*]}" \
  -o APT::FTPArchive::Release::Components=main \
  -o "APT::FTPArchive::Release::Description=Suede official packages" \
  release "$OUTDIR/dists/stable" > "$release_tmp"
mv "$release_tmp" "$OUTDIR/dists/stable/Release"
chmod 644 "$OUTDIR/dists/stable/Release"

# SHA-512 stated rather than left to the key's default: apt rejects SHA-1
# signatures outright.
echo "==> Signing"
(
  cd "$OUTDIR/dists/stable"
  gpg --batch --yes --local-user "$FINGERPRINT" --digest-algo SHA512 \
    --armor --detach-sign -o Release.gpg Release
  gpg --batch --yes --local-user "$FINGERPRINT" --digest-algo SHA512 \
    --clearsign -o InRelease Release
)

echo "==> Exporting the public key"
gpg --batch --export-options export-minimal --export "$FINGERPRINT" \
  > "$OUTDIR/suede-archive-keyring.gpg"
gpg --batch --export-options export-minimal --armor --export "$FINGERPRINT" \
  > "$OUTDIR/suede-archive-keyring.asc"

# Checked the way apt checks it: gpgv against nothing but the keyring being
# published. A signature that only verifies against the private keyring used
# above would pass here and fail on every appliance.
gpgv --keyring "$(realpath "$OUTDIR/suede-archive-keyring.gpg")" \
  "$OUTDIR/dists/stable/InRelease" \
  || die "InRelease does not verify against the published keyring"
gpgv --keyring "$(realpath "$OUTDIR/suede-archive-keyring.gpg")" \
  "$OUTDIR/dists/stable/Release.gpg" "$OUTDIR/dists/stable/Release" \
  || die "Release.gpg does not verify against the published keyring"
echo "    InRelease and Release.gpg verify against suede-archive-keyring.gpg"

# ---------------------------------------------------------------------------
# A landing page, because /apt/ is a URL people will open in a browser.
# Self-contained, and free of anything the docs job's rendered-markdown check
# would mistake for unrendered markdown.
# ---------------------------------------------------------------------------
echo "==> Writing index.html"
fingerprint_spaced=$(sed -E 's/(.{4})/\1 /g; s/ $//' <<<"$FINGERPRINT")
page=$(cat <<'HTML'
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Suede apt repository</title>
<style>
  :root { color-scheme: light dark; --fg: #1d1d1f; --bg: #ffffff; --muted: #5f6368; --code: #f3f3f5; --link: #0b57d0; }
  @media (prefers-color-scheme: dark) {
    :root { --fg: #e8e8ea; --bg: #141416; --muted: #a0a0a8; --code: #222226; --link: #8ab4f8; }
  }
  body { margin: 0; padding: 2rem 16px; background: var(--bg); color: var(--fg);
         font: 16px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
  main { max-width: 46rem; margin: 0 auto; }
  h1 { font-size: 1.6rem; margin: 0 0 0.5rem; }
  p, li { color: var(--fg); }
  .muted { color: var(--muted); }
  a { color: var(--link); }
  code, pre { font: 14px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
  pre { background: var(--code); padding: 1rem; border-radius: 6px; overflow-x: auto; }
  code { background: var(--code); padding: 0.1em 0.3em; border-radius: 4px; }
  pre code { background: none; padding: 0; }
</style>
</head>
<body>
<main>
<h1>Suede apt repository</h1>
<p>Official Suede packages for Debian, Ubuntu and Raspberry Pi OS, on
<code>amd64</code> and <code>arm64</code>. It carries the newest @KEEP@ attested
releases, those whose packages carry a build provenance attestation from the
project's CI; older ones remain on the
<a href="https://github.com/@REPO@/releases">GitHub releases page</a>.</p>

<h2>Add it to a machine</h2>
<pre><code>sudo curl -fsSL https://suede.gameshow.pro/apt/suede-archive-keyring.gpg \
  -o /usr/share/keyrings/suede-archive-keyring.gpg
sudo tee /etc/apt/sources.list.d/suede.sources &gt;/dev/null &lt;&lt;'EOF'
Types: deb
URIs: https://suede.gameshow.pro/apt
Suites: stable
Components: main
Signed-By: /usr/share/keyrings/suede-archive-keyring.gpg
EOF
sudo apt update
sudo apt install suede</code></pre>
<p>Suede is never upgraded automatically. An upgrade restarts the daemon, which
blanks every display it drives, so run <code>sudo apt update &amp;&amp; sudo apt
install suede</code> between shows.</p>
<p>The full instructions are in
<a href="https://suede.gameshow.pro/getting-started/#install">Getting started</a>.</p>

<h2>Signing key</h2>
<p>Fingerprint: <code>@FINGERPRINT@</code></p>
<p><a href="suede-archive-keyring.gpg">suede-archive-keyring.gpg</a> (binary, for
<code>Signed-By</code>) &middot;
<a href="suede-archive-keyring.asc">suede-archive-keyring.asc</a> (armored)</p>
<p class="muted">Generated @DATE@.</p>
</main>
</body>
</html>
HTML
)
page=${page//@FINGERPRINT@/$fingerprint_spaced}
page=${page//@KEEP@/$KEEP}
page=${page//@REPO@/$REPO}
page=${page//@DATE@/$(date -u '+%Y-%m-%d %H:%M UTC')}
printf '%s\n' "$page" > "$OUTDIR/index.html"

# ---------------------------------------------------------------------------
echo "==> Done"
pool_bytes=$(find "$OUTDIR/pool" -type f -printf '%s\n' | awk '{ total += $1 } END { print total + 0 }')
echo "    releases mirrored: ${#versions[@]} ($(printf '%s\n' "${!versions[@]}" | sort -V | paste -sd ' '))"
echo "    packages:          ${#debs[@]}"
echo "    bytes in pool:     $pool_bytes"
echo "    signing key:       $FINGERPRINT"
