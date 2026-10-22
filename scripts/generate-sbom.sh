#!/usr/bin/env bash
# Generate the release's software bill of materials and license notices into
# target/sbom/:
#
#   THIRD_PARTY_LICENSES.txt   every shipped dependency's license text
#                              (cargo-about; about.toml lists the accepted
#                              licenses, and anything else fails here)
#   <crate>.cdx.json           a CycloneDX 1.5 SBOM for each crate that ships a
#                              binary (cargo-cyclonedx)
#   shoestring-wm-<version>-sbom.tar.gz   all of the above, for the release page
#
# The packages install the notices and SBOMs under /usr/share/doc/shoestring-wm/
# (see [package.metadata.deb] and [package.metadata.generate-rpm] in Cargo.toml),
# so run this before `cargo deb` / `cargo generate-rpm`.
#
# Needs cargo-about (with the `cli` feature) and cargo-cyclonedx:
#   cargo install cargo-about --features cli --locked
#   cargo install cargo-cyclonedx --locked
#
#   scripts/generate-sbom.sh [target-triple]   (default: the host's)

set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
out="target/sbom"

# The crates whose binaries the packages install: one SBOM each. The library
# crates (shoestring-config, -ipc, -remote) appear inside these.
binary_crates=(
    shoestring-wm
    shoestring-bar
    shoestring-ctl
    shoestring-tasks
    shoestring-kill
    shoestring-notify
    shoestring-lock
    shoestring-screenshot
    shoestring-confirm
    shoestring-menu
    shoestring-region
    shoestring-mediad
    shoestring-clipboard
    shoestring-remote-server
    shoestring-remote-client
    xdg-desktop-portal-shoestring
)

rm -rf "$out"
mkdir -p "$out"

echo "==> license notices (cargo-about)"
cargo about generate about.hbs -o "$out/THIRD_PARTY_LICENSES.txt"

echo "==> SBOMs (cargo-cyclonedx, $target)"
# cargo-cyclonedx writes <crate>.cdx.json next to each crate's Cargo.toml.
cargo cyclonedx --format json --spec-version 1.5 --target "$target"
for crate in "${binary_crates[@]}"; do
    if [ "$crate" = shoestring-wm ]; then
        src="shoestring-wm.cdx.json"
    else
        src="crates/$crate/$crate.cdx.json"
    fi
    mv "$src" "$out/"
done
# Leave no generated SBOMs in the source tree (the library crates' too).
find . -path ./target -prune -o -name '*.cdx.json' -print0 | xargs -0 -r rm -f

echo "==> archive"
tar -czf "$out/shoestring-wm-$version-sbom.tar.gz" -C "$out" \
    THIRD_PARTY_LICENSES.txt "${binary_crates[@]/%/.cdx.json}"

ls -l "$out"
