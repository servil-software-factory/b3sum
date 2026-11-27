#!/bin/sh
# b3sum's release binary for this machine, built from a fresh clone of the
# release's version commit: statically linked on Linux (the recommended
# build, README.md), as the platform links elsewhere (macOS). Writes
# dist/b3sum-VERSION-TARGET with both its SHA-256 (.sha256) and independent
# pure-Python BLAKE3 (.b3) digest files beside it, for a GitHub release
# (BLAKE3 #446).
#
#     sh tools/b3sum-release.sh
#
# HEAD is the version commit (its tag need not be made yet), or with
# B3SUM_RELEASE_ANY_COMMIT=1 any commit, its binary named by the commit:
# CI builds every candidate so, which keeps this script working.
#
# The same bytes on every machine. On Linux the binary is a function of
# the commit and of tools/b3sum-release.toolchain, whoever builds it and
# wherever, so a release's binary can be checked by building it again:
# our machines and GitHub's must agree before a release, and anyone can
# rebuild what we published. Three things make it so:
# - A fresh clone: a binary is built from every file of its checkout that
#   Git does not ignore, and a working checkout holds more than the
#   commit. 0.9.0's two binaries were built where logs and worktrees lay
#   untracked, and their versions say "dirty" (BLAKE3 #435).
# - One build directory, /tmp/b3sum-release-build: Cargo names each
#   crate's symbols by a hash of its directory. Cargo's and Rust's own
#   directories, which the binary's messages name, are written as /cargo
#   and /rust.
# - The toolchain file's compiler and C library, and Clang as the linker's
#   driver (GCC's passes the linker other flags on AArch64).
set -eu
cd "$(git rev-parse --show-toplevel)"
root=$PWD
commit=$(git rev-parse HEAD)
parent=$(git rev-parse "${commit}^")
target=$(rustc -vV | sed -n 's/^host: //p')
# Read [package].version from the pinned commit's Cargo.toml (not dirty working
# tree or a stray `version =` line in another TOML table; M16).
pkg_version=$(git show "$commit:Cargo.toml" | python3 -c 'import sys, tomllib; print(tomllib.loads(sys.stdin.read())["package"]["version"])')
version=$pkg_version
if [ "${version#*+}" = "$parent" ]; then
	name="b3sum-v$version-$target"
	expected_version="b3sums $version"
elif [ "${B3SUM_RELEASE_ANY_COMMIT:-}" = 1 ]; then
	version=""
	name="b3sum-$commit-$target"
	expected_version="b3sums $pkg_version.$commit"
else
	echo "b3sum-release: HEAD is no version commit (stamp Cargo.toml and Cargo.lock to X.Y.Z+<parent-commit>)" >&2
	exit 1
fi
# Pass compiler arguments through CARGO_ENCODED_RUSTFLAGS (unit-separator 0x1f)
# so spaces in CARGO_HOME, sysroot, or CC never split an argument.
us=$(printf '\037')
encoded_flags="--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo${us}--remap-path-prefix=$(rustc --print sysroot)=/rust"
case "$target" in
*-linux-gnu)
	export CC="${CC:-clang}"
	have="rustc $(rustc -V | cut -d' ' -f2)
libc6-dev $(dpkg-query -W -f='${Version}' libc6-dev)"
	want=$(grep '^rustc \|^libc6-dev ' tools/b3sum-release.toolchain)
	if [ "$have" != "$want" ]; then
		printf 'b3sum-release: this machine has\n%s\nand tools/b3sum-release.toolchain names\n%s\nA release binary is built with exactly those.\n' "$have" "$want" >&2
		exit 1
	fi
	encoded_flags="${encoded_flags}${us}-C${us}target-feature=+crt-static${us}-C${us}linker=$CC"
	;;
esac
# Reserve the fixed reproducible build path exclusively (M19): never delete an
# existing directory we did not create, and remove our own reservation exactly
# once on EXIT (signals exit nonzero so EXIT runs cleanly without racing a
# successor).
build=/tmp/b3sum-release-build
if ! mkdir -m 700 "$build" 2>/dev/null; then
	echo "b3sum-release: $build already exists (another build running or stale dir); refusing to delete an unowned directory" >&2
	exit 1
fi
cleanup() { rm -rf "$build"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

git clone --quiet --no-checkout . "$build/source"
git -C "$build/source" checkout --quiet --detach "$commit"
# Run Cargo from inside the fresh clone so an untracked `.cargo/config.toml` in
# the caller's checkout cannot alter compiler or linker settings.
(cd "$build/source" && CARGO_ENCODED_RUSTFLAGS="$encoded_flags" cargo build --release --locked --target "$target" --target-dir "$build/target")

# Stage and verify everything privately inside `$build/stage` before publishing
# into shared `dist/` so concurrent runs never observe or delete each other's
# unverified artifacts.
stage="$build/stage"
mkdir -m 700 "$stage"
staged_bin="$stage/$name"
cp "$build/target/$target/release/b3sum" "$staged_bin"

# Verify exact `--version` stdout (byte-for-byte including trailing LF) and exit
# status (M16), and for a release commit run release_artifacts.py as well.
printf '%s\n' "$expected_version" > "$stage/version.want"
"$staged_bin" --version > "$stage/version.out"
if ! cmp -s "$stage/version.out" "$stage/version.want"; then
	printf 'b3sum-release: expected --version %s, got:\n' "$expected_version" >&2
	cat "$stage/version.out" >&2
	exit 1
fi
if [ -n "$version" ]; then
	python3 "$root/tools/release_artifacts.py" "v$version" "$staged_bin" --commit "$commit"
fi
# The binary checks itself against BLAKE3's published digest of nothing,
# then both .sha256 and .b3 are written independently of the binary under
# check (tools/blake3_ref.py) and cross-checked against it.
test "$(printf '' | "$staged_bin" --no-names)" = af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
(cd "$stage" && { sha256sum "$name" 2>/dev/null || shasum -a 256 "$name"; } > "$name.sha256")
(cd "$stage" && python3 "$root/tools/blake3_ref.py" "$name" > "$name.b3")
test "$(cd "$stage" && "$staged_bin" "$name")" = "$(cat "$stage/$name.b3")"

# Publish verified artifacts atomically into `dist/`.
mkdir -p dist
for ext in "" ".sha256" ".b3"; do
	tmp="dist/.${name}${ext}.tmp.$$"
	cp "$stage/${name}${ext}" "$tmp"
	mv -f "$tmp" "dist/${name}${ext}"
done
out="dist/$name"
file "$out"
cat "dist/$name.sha256" "dist/$name.b3"
echo "b3sum-release: $root/$out, with .sha256 and .b3 beside it"
