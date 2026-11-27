#!/bin/sh
# Install hash-verified CI binaries (cargo-nextest 0.9.152,
# cargo-deny 0.22.0, cargo-vet 0.10.4) into ${DEST:-${CARGO_HOME:-$HOME/.cargo}/bin},
# dropping third-party installer actions.
#
#     sh tools/ci-tools.sh nextest [cargo-deny] [cargo-vet]
set -eu
DEST=${DEST:-"${CARGO_HOME:-$HOME/.cargo}/bin"}
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "$#" -eq 0 ]; then
  echo "usage: sh tools/ci-tools.sh nextest|cargo-deny|cargo-vet ..." >&2
  exit 2
fi

verify_sha256() {
  sha=$1
  file=$2
  sum="$tmp/sum"
  printf '%s  %s\n' "$sha" "$file" > "$sum"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$sum"
  else
    sha256sum -c "$sum"
  fi
}

verify_blake3() {
  expected=$1
  file=$2
  actual=$(python3 "$SCRIPT_DIR/blake3_ref.py" "$file" | awk '{print $1}')
  if [ "$actual" != "$expected" ]; then
    echo "ci-tools: BLAKE3 mismatch for $file: expected $expected, got $actual" >&2
    exit 1
  fi
}

install_nextest() {
  ver=0.9.152
  case "$(uname -sm)" in
    "Linux x86_64")
      target=x86_64-unknown-linux-gnu
      sha=d039b43c3e0958441b6ad0a39c8884d018b3d2e8fb9928f209074b1100b5e17a
      # BLAKE3: 31690208bdb8da793c0480fc470e82662f69c18edf845465b65099f8027a767c
      ;;
    "Linux aarch64")
      target=aarch64-unknown-linux-gnu
      sha=ca1f81a5b86533eb45ab97e0064aa88e925e53aab9a698ca6331a7cad1f85f07
      # BLAKE3: 5364c97094b42f458661d2b1e0626de7b0bb3beeb2e32ab82129d5d1c39af3ab
      ;;
    "Darwin arm64"|"Darwin x86_64")
      target=universal-apple-darwin
      sha=2fadf57df4243e691f088ee3bff4f4ab9af16da0c95a1db84b5cb1685e098a76
      # BLAKE3: 5a6fafdb3f1e4c18116a600ea264eaae7a615a0723fb9dc9756ff06ceab8cdf0
      ;;
    *) echo "ci-tools: no cargo-nextest binary named for $(uname -sm)" >&2; exit 1 ;;
  esac
  archive="$tmp/cargo-nextest.tar.gz"
  curl --proto '=https' --tlsv1.2 -sSfL -o "$archive" \
    "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-${ver}/cargo-nextest-${ver}-${target}.tar.gz"
  verify_sha256 "$sha" "$archive"
  mkdir -p "$DEST"
  tar -xzf "$archive" -C "$DEST" cargo-nextest
  "$DEST/cargo-nextest" --version
}

install_cargo_deny() {
  ver=0.22.0
  case "$(uname -sm)" in
    "Linux x86_64")
      asset=cargo-deny-${ver}-x86_64-unknown-linux-musl.tar.gz
      sha=98c56e6a408bfa5c3c60744b6015aaa6aa08f743fdeb88386d06fca035bb1786
      b3=fae8c1378eb583fa5a13f9b8c6c4250f8c83fdc64acc8377a45ecf10d89a12db
      ;;
    *) echo "ci-tools: no cargo-deny binary named for $(uname -sm)" >&2; exit 1 ;;
  esac
  archive="$tmp/$asset"
  curl --proto '=https' --tlsv1.2 -sSfL -o "$archive" \
    "https://github.com/EmbarkStudios/cargo-deny/releases/download/${ver}/${asset}"
  verify_sha256 "$sha" "$archive"
  verify_blake3 "$b3" "$archive"
  mkdir -p "$DEST"
  tar -xzf "$archive" -C "$tmp" --strip-components=1 --wildcards '*/cargo-deny'
  install -m 755 "$tmp/cargo-deny" "$DEST/cargo-deny"
  "$DEST/cargo-deny" --version
}

install_cargo_vet() {
  ver=0.10.4
  case "$(uname -sm)" in
    "Linux x86_64")
      asset=cargo-vet-x86_64-unknown-linux-gnu.tar.xz
      sha=e367f8ac156208711511274abab0c309e19ed0f11ef2bbe8a7b0294f824cb032
      b3=aa72ea77963e2134b4b6b36f18ec73645c28c99a0a3ee61595d67ec22411f8d3
      ;;
    *) echo "ci-tools: no cargo-vet binary named for $(uname -sm)" >&2; exit 1 ;;
  esac
  archive="$tmp/$asset"
  curl --proto '=https' --tlsv1.2 -sSfL -o "$archive" \
    "https://github.com/mozilla/cargo-vet/releases/download/v${ver}/${asset}"
  verify_sha256 "$sha" "$archive"
  verify_blake3 "$b3" "$archive"
  mkdir -p "$DEST"
  tar -xJf "$archive" -C "$tmp" --strip-components=1 --wildcards '*/cargo-vet'
  install -m 755 "$tmp/cargo-vet" "$DEST/cargo-vet"
  "$DEST/cargo-vet" --version
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for tool in "$@"; do
  case "$tool" in
    nextest) install_nextest ;;
    cargo-deny) install_cargo_deny ;;
    cargo-vet) install_cargo_vet ;;
    *) echo "ci-tools: unknown tool $tool (nextest, cargo-deny, cargo-vet)" >&2; exit 2 ;;
  esac
done
