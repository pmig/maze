#!/usr/bin/env sh
# Download a pinned Stalwart release for the IMAP integration tests.
#
# Usage: fetch-stalwart.sh <dest-dir>
# Prints the path of the `stalwart` binary. Does nothing if it already exists.
#
# Stalwart 0.15.x is used because 0.16 replaced the TOML configuration with a
# web-UI bootstrap flow that cannot be scripted without its JSON schema.
#
# Test processes run this concurrently. Each one downloads into a private temporary
# folder and moves the binary into place with one rename, so no process ever runs a
# half-written binary ("Text file busy"); `mise run e2e` fetches it once up front.
set -eu

VERSION="v0.15.5"
dest="${1:?usage: fetch-stalwart.sh <dest-dir>}/${VERSION}"
bin="${dest}/stalwart"

if [ -x "${bin}" ]; then
  echo "${bin}"
  exit 0
fi

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target="aarch64-apple-darwin"; sha="cac0eab57a5c5844efa7e4bc206842a5532af11c9f5ff7cf023775bcdbf4b33a" ;;
  Darwin-x86_64) target="x86_64-apple-darwin"; sha="561de3a85ba79d4f0a87f3d15d1a992b88dc108ee8e15c0323f6026f466c7485" ;;
  Linux-x86_64) target="x86_64-unknown-linux-gnu"; sha="38ea325845c6de77e062d3295b107d01cb804205de1f8186a78f6ffdec1a2832" ;;
  Linux-aarch64) target="aarch64-unknown-linux-gnu"; sha="30d5c7a7b1236debe5405db9248c5af3c05cdee7a2fc66f665bfeb6bc8a6f154" ;;
  *) echo "unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "${dest}"
work="$(mktemp -d "${dest}/.fetch.XXXXXX")"
trap 'rm -rf "${work}"' EXIT
archive="${work}/stalwart.tar.gz"
curl -fsSL -o "${archive}" \
  "https://github.com/stalwartlabs/stalwart/releases/download/${VERSION}/stalwart-${target}.tar.gz"

if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${archive}" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "${archive}" | cut -d' ' -f1)"
fi
if [ "${actual}" != "${sha}" ]; then
  echo "checksum mismatch for ${archive}: ${actual}" >&2
  exit 1
fi

tar -xzf "${archive}" -C "${work}" stalwart
chmod +x "${work}/stalwart"
mv -f "${work}/stalwart" "${bin}"
echo "${bin}"
