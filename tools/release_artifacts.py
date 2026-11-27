#!/usr/bin/env python3
"""Check that a release's artifacts are clean builds of its version commit.

    python3 tools/release_artifacts.py TAG FILE...       artifacts on disk
    python3 tools/release_artifacts.py TAG --published   what users download: the GitHub
                                                         release's assets, crates.io's packages
    ... --rebuilt DIR    each b3sum program and each crate is also, byte for byte, the one in DIR
                         (and the one in each folder inside DIR: a builder's each)
    ... --builders A=T1,B=T2  with --rebuilt: the builders are the folders A and B inside DIR,
                              each pinned to its b3sum target (T1, T2) and no others; each must
                              hold every crate, its own target's program, and no other files or
                              symbolic links; DIR holds nothing else
    ... --commit COMMIT  the version commit, where its tag is not made yet

TAG is a release's tag, vX.Y.Z+<code commit>: it names the version commit,
whose parent is the code commit. Run this from a clone that has the tag;
anyone can. It prints one line an artifact. Exit status 0: every artifact
is a clean build of the version commit; 1: one is not, or one a release
has is missing.

The tag's name is the release's source. The 40 hex digits in it (which a
program's --version prints too) are the code commit's hash, and this
check holds the version commit to that commit: the same files, but for
the release's version written where a version stood. So a tag that was
moved, or a version commit with anything else in it, is no release here,
and every check below is then of the source the name commits to. With
--published the version commit must also be in the history of the
remote's main branch, which only a release moves.

Take this checker from a commit you trust, not from the tag's own: the
code commit the name gives (git checkout <those 40 digits>) or main. A
forged version commit could carry a checker that passes it.

What makes an artifact clean, by its kind:

  A b3sum program (b3sum-*): it holds its build version (build_version.rs),
  which is the release's version alone when it was built in a Git
  checkout at the version commit with no file changed or added (or from
  the release's own package, where there is no Git to ask). Every other
  build says more: its commit, and from a dirty tree `.dirty` and the
  tree's id. The file's bytes are searched, so a program for another
  machine is checked without running it. This is the build's own word:
  it catches a mistake, as 0.9.0's was, and not a forgery.

  A b3sum program or a .crate, with --rebuilt DIR: it is also the same
  bytes as a second one made from the version commit on another machine
  (tools/b3sum-release.sh, tools/crates-release.sh) and put in DIR under
  the same name. Each script's output is a function of the commit and of
  the toolchain and lock the repository names, so two honest builders
  agree; an artifact that differs from yours was not made from the
  commit, whatever it says of itself. For a crate this covers the files
  Cargo writes too. After a release, the `release artifacts` workflow
  makes again what was published, and so can anyone. DIR may hold a
  folder for each of several builders (the workflow's jobs each put
  theirs in one): the artifact is then compared with every builder's
  copy, and a builder that made crates made this one. Copied into one
  folder instead, a later builder's file would replace an earlier one's
  unread, and a builder that disagreed would go unseen.

  A .crate: Cargo's record in it (.cargo_vcs_info.json) names the version
  commit and no dirty tree, and every file in it is that commit's file,
  byte for byte. Three files are Cargo's own and are not compared with
  the commit: Cargo.toml (its rewriting of the manifest, whose original,
  Cargo.toml.orig, is compared), Cargo.lock, and the record itself.
  --rebuilt covers those: the whole archive against a second packaging.

Why (BLAKE3 #435): 0.9.0's two b3sum programs were built in checkouts that
held untracked files. They say `dirty`, nothing checked, and nobody saw
until a user's question. No source file differed, as the trees their
versions name showed, but only on our machines.
"""

import collections
import gzip
import hashlib
import importlib.util
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import tomllib
import urllib.error
import urllib.parse
import urllib.request


def _load_blake3_ref():
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "blake3_ref.py")
    spec = importlib.util.spec_from_file_location("blake3_ref", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


blake3_ref = _load_blake3_ref()

REPO = "servil-software-factory/b3sum"
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# The crate a release publishes, by its folder here (the root `b3sums` crate).
CRATE_DIRS = ("",)
# The machines a release has a b3sum program for (README.md, "Installation").
B3SUM_TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu")
# A release version or development-line version stamp.
STAMP = re.compile(rb"\d+\.\d+\.\d+\+(?:[0-9a-f]{40}|dev)")
# Cargo's own files in a .crate: not the commit's.
CARGOS = (".cargo_vcs_info.json", "Cargo.toml", "Cargo.lock")


class NotARelease(Exception):
    """The tag is not a release's (vX.Y.Z+<code commit> on a version commit)."""


class Release:
    """A release by its tag: its version, and the version commit whose
    tree its artifacts are built from. Raises NotARelease for any other
    tag."""

    def __init__(self, tag, repo=None, commit=None):
        """`commit`: the version commit where the caller has it (a
        candidate whose tag is not made yet); else the tag's, in `repo`
        (this checkout's repository unless another is named)."""
        self.tag, self.repo, self.version = tag, repo or ROOT, tag[1:]
        # Numbers as Cargo writes them: no zero before another digit.
        named = re.fullmatch(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\+([0-9a-f]{40})", tag)
        if not named:
            raise NotARelease(f"{tag} is no release tag (vX.Y.Z+<the code commit's 40 hex digits>)")
        if commit is None:
            commit = self.found(tag + "^{commit}")
            if commit is None:
                raise NotARelease(f"this clone has no tag {tag}: fetch it (git fetch origin tag {tag})")
        self.commit = commit
        if self.found(commit + "^") != named.group(1):
            raise NotARelease(f"{tag} names commit {commit}, whose parent is not {named.group(1)}: not a version commit")
        more = self.stamp_problems(named.group(1))
        if more:
            raise NotARelease(f"{tag} names commit {commit}, which is not its parent with the version written in: {more[0]}"
                              + (f" (and {len(more) - 1} more)" if len(more) > 1 else "") + ": not a version commit")

    def stamp_problems(self, code):
        """Why the version commit is not the code commit `code` with the
        release's version written in: nothing where each file is the
        same but for lines in which a version (X.Y.Z+<commit>, or
        X.Y.Z+dev) became this release's. Nothing else belongs in a
        version commit: its hash is not in the tag's name, and the code
        commit's is."""
        problems = []
        changed = False
        allowed_manifests = {os.path.join(folder, "Cargo.toml") if folder else "Cargo.toml" for folder in CRATE_DIRS}
        allowed_stamp_files = allowed_manifests | {"Cargo.lock"}
        changed_blobs = {}
        raw = subprocess.run(["git", "-C", self.repo, "diff-tree", "-r", "--raw", "--no-renames", "-z", code, self.commit], capture_output=True, check=True).stdout
        fields = raw.split(b"\0")
        for entry, path in zip(fields[0::2], fields[1::2]):
            old_mode, new_mode, old, new, status = entry.decode().lstrip(":").split()
            path = path.decode(errors="replace")
            if status != "M" or old_mode != new_mode:
                problems.append(f"{path} is added, removed, or of another kind")
                continue
            before_raw, after_raw = (subprocess.run(["git", "-C", self.repo, "cat-file", "blob", blob], capture_output=True, check=True).stdout for blob in (old, new))
            changed_blobs[path] = (before_raw, after_raw)
            before, after = before_raw.split(b"\n"), after_raw.split(b"\n")
            if len(before) != len(after):
                problems.append(f"{path} has lines added or removed")
                continue
            for number, (was, now) in enumerate(zip(before, after), 1):
                if was == now:
                    continue
                stamps_were, stamps_are = STAMP.findall(was), STAMP.findall(now)
                written = [n for w, n in zip(stamps_were, stamps_are) if w != n]
                # The same count of stamps: with one removed or added, what
                # is left around them can still read the same.
                if len(stamps_were) != len(stamps_are) or STAMP.split(was) != STAMP.split(now) or any(n != self.version.encode() for n in written):
                    problems.append(f"{path} line {number} changes more than a version to this release's")
                    break
                if path not in allowed_stamp_files:
                    problems.append(f"{path} is not a version stamp file (only declared Cargo.toml manifests and Cargo.lock may change)")
                    break
                if written:
                    changed = True
        if not problems and not changed:
            problems.append("changes no version from its parent")
        # Require exact stamped values in every declared crate's Cargo.toml and
        # (when present) in Cargo.lock for local crates ONLY — refusing edits to
        # unrelated dependency versions (such as Git-pinned blake3-servil; M20).
        if not problems:
            crate_names = set()
            for folder in CRATE_DIRS:
                manifest_path = os.path.join(folder, "Cargo.toml") if folder else "Cargo.toml"
                manifest_bytes = self.file(manifest_path)
                if manifest_bytes is None:
                    problems.append(f"{manifest_path} is missing in {self.commit}")
                    continue
                try:
                    after_doc = tomllib.loads(manifest_bytes.decode())
                    pkg = after_doc["package"]
                    got_ver = pkg["version"]
                    crate_names.add(pkg["name"])
                except (UnicodeDecodeError, tomllib.TOMLDecodeError, KeyError, TypeError) as e:
                    problems.append(f"{manifest_path} has no valid [package].version ({e})")
                    continue
                if got_ver != self.version:
                    problems.append(f"{manifest_path} [package].version is {got_ver!r}, expected {self.version!r}")
                if manifest_path in changed_blobs:
                    try:
                        before_doc = tomllib.loads(changed_blobs[manifest_path][0].decode())
                    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as e:
                        problems.append(f"parent {manifest_path} could not be parsed ({e})")
                        continue
                    # Only [package].version may change in a declared manifest.
                    expected_doc = dict(before_doc)
                    expected_pkg = dict(before_doc.get("package", {}))
                    expected_pkg["version"] = self.version
                    expected_doc["package"] = expected_pkg
                    if after_doc != expected_doc:
                        problems.append(f"{manifest_path} changes fields other than [package].version")
            lock_bytes = self.file("Cargo.lock")
            if lock_bytes is not None and crate_names:
                try:
                    after_lock = tomllib.loads(lock_bytes.decode())
                    lock_pkgs = after_lock.get("package", [])
                except (UnicodeDecodeError, tomllib.TOMLDecodeError) as e:
                    problems.append(f"Cargo.lock could not be parsed ({e})")
                    lock_pkgs = []
                for cname in sorted(crate_names):
                    local_entries = [p for p in lock_pkgs if isinstance(p, dict) and p.get("name") == cname and "source" not in p]
                    if not local_entries:
                        problems.append(f"Cargo.lock has no local [[package]] entry for {cname}")
                    elif any(p.get("version") != self.version for p in local_entries):
                        found_vers = [p.get("version") for p in local_entries]
                        problems.append(f"Cargo.lock entry for {cname} has version {found_vers!r}, expected {self.version!r}")
                if "Cargo.lock" in changed_blobs and not problems:
                    try:
                        before_lock = tomllib.loads(changed_blobs["Cargo.lock"][0].decode())
                    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as e:
                        problems.append(f"parent Cargo.lock could not be parsed ({e})")
                    else:
                        before_pkgs = before_lock.get("package", [])
                        if len(before_pkgs) != len(lock_pkgs):
                            problems.append("Cargo.lock changed package count")
                        else:
                            for old_p, new_p in zip(before_pkgs, lock_pkgs):
                                if old_p == new_p:
                                    continue
                                is_local_release_crate = (
                                    isinstance(old_p, dict)
                                    and isinstance(new_p, dict)
                                    and old_p.get("name") in crate_names
                                    and "source" not in old_p
                                    and "source" not in new_p
                                )
                                expected_p = dict(old_p, version=self.version) if is_local_release_crate else old_p
                                if new_p != expected_p:
                                    problems.append(f"Cargo.lock changes non-release package {old_p.get('name')!r}")
                                    break
        return problems

    def on_main(self):
        """Whether the version commit is in the history of the remote's
        main branch: True or False, or None where this clone has no
        origin/main to ask."""
        if self.found("refs/remotes/origin/main") is None:
            return None
        return subprocess.run(["git", "-C", self.repo, "merge-base", "--is-ancestor", self.commit, "refs/remotes/origin/main"]).returncode == 0

    def found(self, revision):
        """The commit `revision` names here, or None."""
        out = subprocess.run(["git", "-C", self.repo, "rev-parse", "--verify", "--quiet", revision], capture_output=True, text=True)
        return out.stdout.strip() if out.returncode == 0 else None

    def git(self, *args):
        return subprocess.run(["git", "-C", self.repo, *args], capture_output=True, check=True).stdout.decode().strip()

    def file(self, path):
        """The bytes of `path` in the version commit, through any symbolic
        links (Cargo packages what a link points at); None where the
        commit has no such file."""
        for _ in range(8):
            path = os.path.normpath(path)
            if path.startswith("..") or os.path.isabs(path):
                return None
            entry = self.git("ls-tree", self.commit, "--", path)
            if not entry:
                return None
            mode, kind, blob = entry.split("\t")[0].split()
            if kind != "blob":
                return None
            data = subprocess.run(["git", "-C", self.repo, "cat-file", "blob", blob], capture_output=True, check=True).stdout
            if mode != "120000":
                return data
            path = os.path.join(os.path.dirname(path), data.decode())
        return None

    def crate(self, folder):
        """The name of the crate in `folder`, from the commit's manifest."""
        manifest_path = os.path.join(folder, "Cargo.toml")
        manifest = self.file(manifest_path)
        if manifest is None:
            raise NotARelease(f"{self.tag}'s commit has no {manifest_path}")
        try:
            name = tomllib.loads(manifest.decode())["package"]["name"]
        except (UnicodeDecodeError, tomllib.TOMLDecodeError, KeyError, TypeError) as e:
            raise NotARelease(f"{self.tag}'s {manifest_path} has no [package].name ({e})") from e
        if not isinstance(name, str) or not name:
            raise NotARelease(f"{self.tag}'s {manifest_path} has an invalid [package].name")
        return name


def said_beyond(data, version):
    """What the build versions in `data` say beyond `version`, each once:
    nothing for a clean build of the version commit. None where `data`
    holds no build version of this release at all."""
    pattern = re.compile(re.escape(version.encode()) + rb"(\.unverified|(?:\.[0-9a-f]{40})?(?:\.dirty(?:\.[0-9a-f]{40})?)?)")
    found = [m.group(1).decode() for m in pattern.finditer(data)]
    return sorted({more for more in found if more}) if found else None


def program_problems(data, release):
    """Why the b3sum program `data` is no clean build of the release."""
    said = said_beyond(data, release.version)
    if said is None:
        return [f"holds no build version of {release.version}: not a build of this release"]
    problems = []
    for more in said:
        if more == ".unverified":
            problems.append(f"its build version is {release.version}{more}: built with unreadable or malformed VCS metadata")
            continue
        commit, dirty, tree = more.partition(".dirty")
        commit, tree = commit.lstrip("."), tree.lstrip(".")
        where = "the version commit" if commit in ("", release.commit) else f"another commit ({commit[:12]})"
        what = f"built at {where}" + (" from a dirty tree, files changed or added" + (f" (its tree: {tree})" if tree else "") if dirty else "")
        problems.append(f"its build version is {release.version}{more}: {what}")
    return problems


# Hard bounds on bytes read/expanded while inspecting a single artifact so a
# malformed or unexpectedly huge HTTP response or `.crate` archive cannot
# exhaust memory (`b3sum` static tarballs are ~2–4 MiB; `.crate` is < 1 MiB).
MAX_ARTIFACT_BYTES = 64 * 1024 * 1024
MAX_CRATE_UNCOMPRESSED_BYTES = 32 * 1024 * 1024
MAX_CRATE_MEMBERS = 4096
_READ_CHUNK_BYTES = 64 * 1024


def _read_bounded(stream, limit, label):
    """Read `stream` in chunks, stopping with ValueError if it exceeds `limit` bytes."""
    chunks = []
    total = 0
    while True:
        chunk = stream.read(min(_READ_CHUNK_BYTES, limit - total + 1))
        if not chunk:
            return b"".join(chunks)
        total += len(chunk)
        if total > limit:
            raise ValueError(f"{label} exceeds maximum allowed size ({limit} bytes)")
        chunks.append(chunk)


def crate_problems(data, release, max_uncompressed_bytes=MAX_CRATE_UNCOMPRESSED_BYTES, max_members=MAX_CRATE_MEMBERS):
    """Why the .crate `data` is not the version commit's files."""
    # Cap the decompressed gzip stream BEFORE handing it to tarfile so a
    # gzip/header bomb cannot exhaust memory or CPU during archive traversal (R3 M5).
    try:
        with gzip.GzipFile(fileobj=io.BytesIO(data), mode="rb") as gz:
            raw_tar = _read_bounded(gz, max_uncompressed_bytes, "decompressed crate")
    except ValueError:
        return [f"expands past maximum allowed size ({max_uncompressed_bytes} bytes)"]
    with tarfile.open(fileobj=io.BytesIO(raw_tar), mode="r:") as archive:
        members = []
        odd = []
        names = []
        files = {}
        remaining = max_uncompressed_bytes
        for m in archive:
            members.append(m)
            if len(members) > max_members:
                return [f"holds more than {max_members} entries"]
            if not (m.isfile() or m.isdir()):
                odd.append(m.name)
                continue
            if m.isfile():
                names.append(m.name)
                if "/" in m.name:
                    try:
                        content = _read_bounded(archive.extractfile(m), remaining, m.name)
                    except ValueError:
                        return [f"expands past maximum allowed size ({max_uncompressed_bytes} bytes)"]
                    remaining -= len(content)
                    files[m.name.split("/", 1)[1]] = content
        if odd:
            return [f"holds what is no plain file ({odd[0]})"]
        # Every file once, and in the one folder Cargo packs a crate into: a
        # second entry of a name, or one outside the folder, is a file some
        # reader of the archive sees and this check would not.
        counts = collections.Counter(names)
        twice = sorted(n for n, c in counts.items() if c > 1)
        if twice:
            return [f"holds {twice[0]} more than once"]
        folders = sorted({n.split("/", 1)[0] if "/" in n else "" for n in names} | {m.name.split("/", 1)[0] for m in members if m.isdir()})
        if len(folders) != 1 or not folders[0]:
            return [f"holds files outside one folder ({', '.join(f or 'the top' for f in folders)})"]
        if any(os.path.normpath(n) != n or n.startswith("/") for n in names):
            return ["holds a file whose path is not plain"]
    for cargos in (".cargo_vcs_info.json", "Cargo.toml.orig"):
        if cargos not in files:
            return [f"has no {cargos}: not as Cargo packages a crate from a Git checkout"]
    record = json.loads(files[".cargo_vcs_info.json"])
    problems = []
    if record.get("git", {}).get("sha1") != release.commit:
        problems.append(f"packaged at commit {record.get('git', {}).get('sha1')}, not the version commit")
    if record.get("git", {}).get("dirty"):
        problems.append("Cargo packaged it from a dirty tree")
    folder = record.get("path_in_vcs", "")
    # A readme outside the crate's folder (`readme = "../README.md"`): Cargo
    # copies it to the package's top under its own name.
    readme = re.search(r'(?m)^readme\s*=\s*"([^"]+)"', files["Cargo.toml.orig"].decode(errors="replace"))
    outside = os.path.normpath(os.path.join(folder, readme.group(1))) if readme and readme.group(1).startswith("..") else None
    for path, got in sorted(files.items()):
        if path in CARGOS:
            continue
        in_commit = os.path.join(folder, "Cargo.toml" if path == "Cargo.toml.orig" else path)
        want = release.file(in_commit)
        if want is None and outside and path == os.path.basename(outside):
            in_commit, want = outside, release.file(outside)
        if want is None:
            problems.append(f"{path} is no file of the version commit")
        elif want != got:
            problems.append(f"{path} differs from the version commit's {in_commit}")
    return problems


def problems(name, data, release):
    """Why the artifact `name`, with bytes `data`, is no clean build of the
    release's version commit: nothing for one that is. A kind this check
    does not know is a problem, never skipped."""
    if name.endswith(".crate"):
        return crate_problems(data, release)
    # b3sum as tools/b3sum-release.sh names it, for some machine: a file of
    # another name beside it (b3sum-...-x86_64-unknown-linux-gnu.b3) is
    # not taken for the program.
    if re.fullmatch(re.escape(f"b3sum-{release.tag}-") + r"[A-Za-z0-9_-]+", name):
        return program_problems(data, release)
    return ["an artifact of a kind this check does not know (a b3sum program, a .crate)"]


def expected(release):
    """The file names a release has, as tools/b3sum-release.sh and Cargo
    name them: b3sum for each target machine, and the b3sums crate."""
    programs = [f"b3sum-{release.tag}-{target}" for target in B3SUM_TARGETS]
    return programs + [f"{release.crate(folder)}-{release.version}.crate" for folder in CRATE_DIRS]


def expected_program_digests(release):
    """The .sha256 and .b3 digest sidecars required beside every b3sum
    program of `release` (tools/b3sum-release.sh, BLAKE3 #446, #465)."""
    programs = [n for n in expected(release) if not n.endswith(".crate")]
    return [n + suffix for n in programs for suffix in DIGEST_SUFFIXES]


def _plain_file(path):
    return os.path.isfile(path) and not os.path.islink(path)


def _plain_dir(path):
    return os.path.isdir(path) and not os.path.islink(path)


def read(path, max_bytes=MAX_ARTIFACT_BYTES):
    with open(path, "rb") as f:
        return _read_bounded(f, max_bytes, path)


DIGEST_SUFFIXES = (".sha256", ".b3")


def is_digest(name):
    """A digest file beside an artifact (tools/b3sum-release.sh and
    tools/crates-release.sh write .sha256 and .b3)."""
    return name.endswith(DIGEST_SUFFIXES)


def _artifact_digests(data):
    return (hashlib.sha256(data).hexdigest(), blake3_ref.blake3_hex(data))


def digest_problems(name, data, seen):
    """Why the digest file `name` is not the SHA-256 or BLAKE3 line of the
    artifact it is named for, among those checked before it (`seen`, mapping
    artifact name to `(sha256_hex, blake3_hex)` or raw `bytes`)."""
    if name.endswith(".sha256"):
        of = name[: -len(".sha256")]
        if of not in seen:
            return [f"is the digest of {of}, which is no artifact here"]
        entry = seen[of]
        sha_hex = entry[0] if isinstance(entry, tuple) else hashlib.sha256(entry).hexdigest()
        want = f"{sha_hex}  {of}\n".encode()
        return [] if data == want else [f"is not the SHA-256 line of {of}"]
    if name.endswith(".b3"):
        of = name[: -len(".b3")]
        if of not in seen:
            return [f"is the digest of {of}, which is no artifact here"]
        entry = seen[of]
        b3_hex = entry[1] if isinstance(entry, tuple) else blake3_ref.blake3_hex(entry)
        want = f"{b3_hex}  {of}\n".encode()
        return [] if data == want else [f"is not the BLAKE3 line of {of}"]
    return ["is not a recognized digest file"]


def fetch(url, max_bytes=MAX_ARTIFACT_BYTES):
    # (A search of the whole address for GitHub's name once sent the
    # token wherever a path or a longer host held the name.)
    with urllib.request.urlopen(request_for(url), timeout=300) as response:
        return _read_bounded(response, max_bytes, url)


def request_for(url):
    """The request for `url`. The token goes to GitHub's API alone, told
    by the address's host, and is a header that is not sent on to where
    a redirect leads: an ordinary header is, whatever host that is."""
    request = urllib.request.Request(url, headers={"User-Agent": f"{REPO} tools/release_artifacts.py"})
    where = urllib.parse.urlsplit(url)
    if (where.scheme, where.hostname) == ("https", "api.github.com") and os.environ.get("GITHUB_TOKEN"):
        request.add_unredirected_header("Authorization", "Bearer " + os.environ["GITHUB_TOKEN"])
    return request


def published(release):
    """What users download for the release, each as (name, bytes or None
    where it is not there): the b3sums crate from crates.io, and every
    asset of the GitHub release, each program's digest files among them."""
    for folder in CRATE_DIRS:
        crate = release.crate(folder)
        name = f"{crate}-{release.version}.crate"
        try:
            yield f"crates.io: {name}", name, fetch(f"https://static.crates.io/crates/{crate}/{urllib.parse.quote(name)}")
        except urllib.error.HTTPError as e:
            if e.code not in (403, 404):
                raise
            e.close()
            yield f"crates.io: {name}", name, None
    try:
        listing = json.loads(fetch(f"https://api.github.com/repos/{REPO}/releases/tags/{urllib.parse.quote(release.tag)}"))
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
        e.close()
        yield f"the GitHub release {release.tag}", release.tag, None
        return
    names = [asset["name"] for asset in listing["assets"]]
    # Each artifact before its digest file, which is checked against it.
    for asset in sorted(listing["assets"], key=lambda asset: (is_digest(asset["name"]), asset["name"])):
        yield f"the GitHub release: {asset['name']}", asset["name"], fetch(asset["browser_download_url"])
    # Each program, and both digest files the release has beside it
    # (CONTRIBUTING, tools/b3sum-release.sh): a missing digest file fails,
    # as a wrong one does (BLAKE3 #446).
    programs = [n for n in expected(release) if not n.endswith(".crate")]
    for missing in [n for n in programs + expected_program_digests(release) if n not in names]:
        yield f"the GitHub release: {missing}", missing, None


def _parse_builders(named):
    """Parse `named` builder specs (`NAME=TARGET`), returning (specs, error)."""
    specs, seen_names, seen_targets = [], set(), set()
    for item in named:
        sub, sep, target = item.partition("=")
        if not sep or sub in ("", ".", "..") or os.path.basename(sub) != sub or sub in seen_names:
            return None, f"the builders named are not NAME=TARGET pairs with distinct folder names: {', '.join(named)}"
        if target not in B3SUM_TARGETS or target in seen_targets:
            return None, f"the builders named do not assign each release target once: {', '.join(named)}"
        seen_names.add(sub)
        seen_targets.add(target)
        specs.append((sub, target))
    if seen_targets != set(B3SUM_TARGETS):
        missing = sorted(set(B3SUM_TARGETS) - seen_targets)
        return None, f"the builders named cover no builder for {', '.join(missing)}"
    return specs, None


def rebuilt_problems(name, data, rebuilt, named=None, release=None):
    """Why the b3sum program or crate `name` is not the second one in the
    folder `rebuilt`: nothing where the two are the same bytes. Every file
    and folder inspected under `rebuilt` must be real (never a symbolic
    link). With `named` (`NAME=TARGET` per builder), `rebuilt` holds those
    builder folders and nothing else, each builder folder holds only plain
    files belonging to that builder (its own target's b3sum program, every
    release crate, and optional .sha256/.b3 sidecars of those), and each
    b3sum program is compared specifically against its target's builder
    (BLAKE3 #465)."""
    if not _plain_dir(rebuilt):
        return [f"{rebuilt} is not a real directory"]
    if named is None:
        links = sorted(sub for sub in os.listdir(rebuilt) if os.path.islink(os.path.join(rebuilt, sub)))
        if links:
            return [f"{rebuilt} holds a symbolic link ({links[0]}): every rebuilt file and folder is real"]
        builders = [rebuilt] + sorted(os.path.join(rebuilt, sub) for sub in os.listdir(rebuilt) if _plain_dir(os.path.join(rebuilt, sub)))
        for builder in builders[1:]:
            sub_links = sorted(e for e in os.listdir(builder) if os.path.islink(os.path.join(builder, e)))
            if sub_links:
                return [f"{builder} holds a symbolic link ({sub_links[0]}): every rebuilt file is a plain file"]
        copies = [builder for builder in builders if _plain_file(os.path.join(builder, name))]
        if not copies:
            script = "crates-release" if name.endswith(".crate") else "b3sum-release"
            return [f"no second build of it in {rebuilt} to compare with (sh tools/{script}.sh at the version commit, on another machine)"]
        found = []
        for builder in copies:
            other = read(os.path.join(builder, name))
            if other != data:
                digests = [hashlib.sha256(d).hexdigest() for d in (data, other)]
                found.append(f"its bytes (SHA-256 {digests[0]}) are not the second build's in {builder} (SHA-256 {digests[1]}): one of the two was not made from the version commit by the release's script")
        if name.endswith(".crate"):
            for builder in builders:
                if builder not in copies and any(_plain_file(os.path.join(builder, n)) and n.endswith(".crate") for n in os.listdir(builder)):
                    found.append(f"{builder} holds crates and not this one: its builder packaged fewer than the release has")
        return found

    specs, err = _parse_builders(named)
    if err is not None:
        return [err]
    builder_names = [sub for sub, _ in specs]
    builders = [(os.path.join(rebuilt, sub), target) for sub, target in specs]
    absent = [builder for builder, _ in builders if not _plain_dir(builder)]
    if absent:
        return [f"no files of the builder {os.path.basename(builder)} in {rebuilt}: each builder named has a folder of its own there" for builder in absent]
    others = sorted(sub for sub in os.listdir(rebuilt) if sub not in builder_names)
    if others:
        return [f"{rebuilt} holds entries outside the builders named ({', '.join(others)}): with builders named, every copy is one builder's"]
    crate_names = {f"{release.crate(folder)}-{release.version}.crate" for folder in CRATE_DIRS} if release is not None else None
    for builder, target in builders:
        entries = sorted(os.listdir(builder))
        non_plain = [e for e in entries if not _plain_file(os.path.join(builder, e))]
        if non_plain:
            return [f"{builder} holds what is no plain file ({non_plain[0]}): every rebuilt file is a plain file"]
        if release is not None:
            own_program = f"b3sum-{release.tag}-{target}"
            allowed = {own_program, *crate_names}
            allowed |= {n + suffix for n in allowed for suffix in DIGEST_SUFFIXES}
            extra = [e for e in entries if e not in allowed]
            if extra:
                return [f"{builder} holds an unexpected file ({extra[0]}): a builder holds only its own target's program, the release's crates, and their digests"]
            if own_program not in entries:
                return [f"{builder} holds no {own_program}: each builder named builds its own target's program"]
    if name.endswith(".crate"):
        target_builders = [builder for builder, _ in builders]
    else:
        matched = [builder for builder, target in builders if name.endswith("-" + target)]
        target_builders = matched or [builder for builder, _ in builders]
    found = []
    copies = [builder for builder in target_builders if _plain_file(os.path.join(builder, name))]
    if not copies:
        script = "crates-release" if name.endswith(".crate") else "b3sum-release"
        return [f"no second build of it in {rebuilt} to compare with (sh tools/{script}.sh at the version commit, on another machine)"]
    for builder in copies:
        other = read(os.path.join(builder, name))
        if other != data:
            digests = [hashlib.sha256(d).hexdigest() for d in (data, other)]
            found.append(f"its bytes (SHA-256 {digests[0]}) are not the second build's in {builder} (SHA-256 {digests[1]}): one of the two was not made from the version commit by the release's script")
    if name.endswith(".crate"):
        for builder in target_builders:
            if builder not in copies:
                found.append(f"{builder} does not hold this crate: every builder named packages every crate of the release")
    return found


def check(items, release, out=None, rebuilt=None, builders=None):
    """Print a line for each artifact (more for one with several
    problems); how many are not clean or not there. With `rebuilt`, a
    folder, each b3sum program and crate is also the second one there;
    `builders`: rebuilt_problems' `named`."""
    bad = 0
    seen = {}
    for label, name, data in items:
        if data is None:
            lines = [f"MISSING    {label}"]
        elif is_digest(name):
            lines = [f"NOT CLEAN  {label}: {problem}" for problem in digest_problems(name, data, seen)]
        else:
            seen[name] = _artifact_digests(data)
            found = problems(name, data, release)
            if rebuilt is not None and (name.startswith("b3sum-") or name.endswith(".crate")):
                found = found + rebuilt_problems(name, data, rebuilt, builders, release=release)
            lines = [f"NOT CLEAN  {label}: {problem}" for problem in found]
        bad += bool(lines)
        print("\n".join(lines or [f"clean      {label}"]), file=out or sys.stdout)
    return bad


def directory(path, release, rebuilt=None, builders=None):
    """A release's artifacts gathered in the folder `path` before anything
    is published (the files there that carry its version): (True, [])
    where every one a release has (including each program's .sha256 and
    .b3 sidecars) is there as a plain file and each is clean; (False, the
    lines of those that are not clean); (None, the names not there yet).
    `rebuilt`, `builders`: as check's."""
    if os.path.islink(path):
        return False, [f"NOT CLEAN  {path}: is a symbolic link"]
    names = sorted((n for n in os.listdir(path) if release.version in n), key=lambda n: (is_digest(n), n)) if os.path.isdir(path) else []
    non_plain = [n for n in names if not _plain_file(os.path.join(path, n))]
    if non_plain:
        return False, [f"NOT CLEAN  {n}: is no plain file" for n in non_plain]
    report = io.StringIO()
    if check(((n, n, read(os.path.join(path, n))) for n in names), release, out=report, rebuilt=rebuilt, builders=builders):
        return False, [line for line in report.getvalue().splitlines() if not line.startswith("clean")]
    want = expected(release) + expected_program_digests(release)
    missing = [n for n in want if n not in names]
    return (None, missing) if missing else (True, [])


def main(argv):
    argv, options = list(argv), {}
    for option in ("--rebuilt", "--commit", "--builders"):
        if option in argv[:-1]:
            at = argv.index(option)
            options[option] = argv[at + 1]
            del argv[at : at + 2]
    paths = sorted(argv[1:], key=is_digest)
    builders = options["--builders"].split(",") if "--builders" in options else None
    if not paths or argv[0].startswith("-") or any(path.startswith("--") for path in paths if path != "--published") or (builders is not None and "--rebuilt" not in options):
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        sys.exit(2)
    try:
        release = Release(argv[0], commit=options.get("--commit"))
        bad = 0
        if paths == ["--published"]:
            items = published(release)
            # Published from main: the branch only a release moves.
            on_main = release.on_main()
            if on_main is not True:
                bad = 1
                print(f"NOT CLEAN  the tag: its commit {release.commit} is not in origin/main's history" if on_main is False
                      else "MISSING    origin/main in this clone, whose history a release's commit is in (git fetch origin main)")
        else:
            items = ((path, os.path.basename(path), read(path)) for path in paths)
        bad += check(items, release, rebuilt=options.get("--rebuilt"), builders=builders)
    except NotARelease as e:
        sys.exit(f"release_artifacts: {e}")
    print(f"release_artifacts: {release.tag} is commit {release.commit}: " + (f"{bad} artifacts are not clean builds of it, or are missing" if bad else "every artifact is a clean build of it"))
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main(sys.argv[1:])
