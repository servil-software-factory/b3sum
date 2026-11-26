#!/bin/sh
# The b3sums crate archive for a release, packaged from a fresh clone of
# the version commit with the dependency versions the repository names and
# no others. Writes dist/b3sums-VERSION.crate with its .sha256 and .b3
# digest files beside it.
#
#     sh tools/crates-release.sh [--no-verify]
#
# HEAD is the version commit, or with RELEASE_ANY_COMMIT=1 any commit:
# CI packages every candidate so. --no-verify leaves out Cargo's build of
# the packaged crate (the release's own run keeps it).
#
# The same bytes whoever packages. An archive is a function of the commit,
# of Cargo.lock, and of Cargo's version, so a release's crate can be
# checked by packaging it again (tools/release_artifacts.py --rebuilt):
# ours must equal GitHub's before a release, and anyone can repackage what
# crates.io serves. What makes it so:
# - A fresh clone of the commit in a private temporary directory outside
#   the caller's checkout (so version, toolchain pin, helper scripts, and
#   Cargo config all come from that commit, never the caller's tree).
# - Cargo.lock from that commit, and --frozen: Cargo resolves nothing and
#   asks the network nothing while it packages.
# - The rustc and matching sysroot cargo that tools/b3sum-release.toolchain
#   names: Cargo rewrites Cargo.toml as it packages, in its version's way.
set -eu
cd "$(git rev-parse --show-toplevel)"
root=$PWD
verify=""
case "${1:-}" in
--no-verify) verify="--no-verify" ;;
"") ;;
*) echo "crates-release: no option $1 (--no-verify)" >&2; exit 2 ;;
esac
commit=$(git rev-parse HEAD)
parent=$(git rev-parse --verify --quiet HEAD^ || true)
stage=$(mktemp -d /tmp/b3sum-crates-release.XXXXXX)
trap 'rm -rf "$stage"' EXIT INT TERM
chmod 700 "$stage"
git clone --quiet --no-checkout . "$stage/checkout"
git -C "$stage/checkout" checkout --quiet --detach "$commit"
version=$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["package"]["version"])' "$stage/checkout/Cargo.toml")
is_release_commit=""
if [ -n "$parent" ] && [ "${version#*+}" = "$parent" ]; then
	is_release_commit=1
elif [ "${RELEASE_ANY_COMMIT:-${B3SUM_RELEASE_ANY_COMMIT:-}}" != 1 ]; then
	echo "crates-release: HEAD is no version commit (version X.Y.Z+<parent commit>)" >&2
	exit 1
fi
have="rustc $(rustc -V | cut -d' ' -f2)"
want=$(grep '^rustc ' "$stage/checkout/tools/b3sum-release.toolchain")
if [ "$have" != "$want" ]; then
	echo "crates-release: this machine has $have, and tools/b3sum-release.toolchain names $want: a release's crate is packaged with exactly that" >&2
	exit 1
fi
sysroot=$(rustc --print sysroot)
cargo_bin="$sysroot/bin/cargo"
rustc_bin="$sysroot/bin/rustc"
if [ ! -x "$cargo_bin" ] || [ ! -x "$rustc_bin" ]; then
	echo "crates-release: expected executable cargo and rustc in verified sysroot $sysroot/bin" >&2
	exit 1
fi
cp "$stage/checkout/Cargo.lock" "$stage/Cargo.lock.orig"
# Run Cargo inside the staged checkout (outside the caller's tree) so Cargo
# cannot inherit a caller-only .cargo/config.toml and uses any committed
# config in the release commit. Fetch locked dependencies first, then
# package strictly frozen with the verified sysroot cargo and rustc.
(
	cd "$stage/checkout"
	RUSTC="$rustc_bin" "$cargo_bin" fetch --quiet --locked
	# shellcheck disable=SC2086
	RUSTC="$rustc_bin" "$cargo_bin" package --frozen $verify --target-dir "$stage/target"
)
cmp -s "$stage/Cargo.lock.orig" "$stage/checkout/Cargo.lock" || { echo "crates-release: packaging changed Cargo.lock" >&2; exit 1; }
mkdir -p dist
cp "$stage"/target/package/*.crate dist/
(
	cd dist
	for crate in "$stage"/target/package/*.crate; do
		base=$(basename "$crate")
		{ sha256sum "$base" 2>/dev/null || shasum -a 256 "$base"; } > "$base.sha256"
		python3 "$stage/checkout/tools/blake3_ref.py" "$base" > "$base.b3"
		cat "$base.sha256" "$base.b3"
	done
)
if [ -n "$is_release_commit" ]; then
	for crate in "$stage"/target/package/*.crate; do
		base=$(basename "$crate")
		python3 "$stage/checkout/tools/release_artifacts.py" "v$version" "dist/$base" --commit "$commit" || {
			rm -f "dist/$base" "dist/$base.sha256" "dist/$base.b3"
			exit 1
		}
	done
fi
echo "crates-release: the b3sums archive (with .sha256 and .b3) is in $root/dist"
