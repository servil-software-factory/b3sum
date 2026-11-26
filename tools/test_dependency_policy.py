"""python3 tools/test_dependency_policy.py: unit tests for tools/dependency_policy.py."""
import os
import sys
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dependency_policy as policy

ROOT = Path("/repo")
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def package(name, folder=None, source=None):
    place = ROOT / folder if folder else ROOT / "registry" / name
    return {"id": f"{name}#1.0.0", "name": name, "version": "1.0.0", "source": source,
            "manifest_path": str(place / "Cargo.toml")}


OURS = [package("b3sums", ".")]
OUR_IDS = [p["id"] for p in OURS]


class Strangers(unittest.TestCase):
    def test_our_crate_and_registry_packages_are_no_strangers(self):
        self.assertEqual(policy.strangers({"packages": OURS + [package("libc", source=REGISTRY)], "workspace_members": OUR_IDS}, root=ROOT), [])

    def test_a_vendored_path_crate_is_named_with_its_folder(self):
        wrong = policy.strangers({"packages": OURS + [package("copied", "third-party/copied")], "workspace_members": OUR_IDS}, root=ROOT)
        self.assertEqual(len(wrong), 1)
        self.assertIn("copied 1.0.0 is taken by path, from third-party/copied/", wrong[0])

    def test_an_implicit_second_workspace_member_is_caught(self):
        extra = package("copied", "copied")
        wrong = policy.strangers({"packages": OURS + [extra], "workspace_members": OUR_IDS + [extra["id"]]}, root=ROOT)
        self.assertTrue(any("Cargo workspace_members are" in w for w in wrong), wrong)
        self.assertTrue(any("copied 1.0.0 is taken by path" in w for w in wrong), wrong)

    def test_an_expected_name_in_another_folder_is_a_stranger(self):
        moved = package("b3sums", "copy/b3sums")
        wrong = policy.strangers({"packages": [moved], "workspace_members": [moved["id"]]}, root=ROOT)
        self.assertEqual(len(wrong), 2)
        self.assertIn("b3sums 1.0.0 is taken by path, from copy/b3sums/", wrong[0])
        self.assertIn("expected crate b3sums at ./Cargo.toml is absent", wrong[1])

    def test_another_name_in_the_root_folder_is_a_stranger(self):
        other = package("other", ".")
        wrong = policy.strangers({"packages": [other], "workspace_members": [other["id"]]}, root=ROOT)
        self.assertTrue(any("other 1.0.0 is taken by path, from ./" in w for w in wrong), wrong)
        self.assertTrue(any("expected crate b3sums at ./Cargo.toml is absent" in w for w in wrong), wrong)

    def test_a_missing_crate_is_said(self):
        wrong = policy.strangers({"packages": [], "workspace_members": []}, root=ROOT)
        self.assertTrue(any("expected crate b3sums at ./Cargo.toml is absent" in w for w in wrong), wrong)


class StripEvidence(unittest.TestCase):
    def test_strips_audits_exemptions_imports_and_trust_while_keeping_neighbours(self):
        sample = (
            '[policy.libc]\nnotes = """\n[not.a.header]\n"""\n\n'
            '[[exemptions.libc]]\nversion = "0.2.191"\ncriteria = "agent-safe-to-deploy"\n\n'
            '[[audits.libc]]\nwho = "Devon"\ncriteria = "agent-safe-to-deploy"\nversion = "0.2.191"\n\n'
            '[[audits.google.audits.libc]]\ncriteria = "safe-to-deploy"\nversion = "0.2.191"\n\n'
            '[[trusted.libc]]\nuser-id = 1\n\n'
            '[[exemptions.libc-other]]\nversion = "0.1.0"\ncriteria = "agent-safe-to-run"\n'
        )
        got = policy.strip_toml_crate_sections(sample, "libc")
        self.assertIn("[policy.libc]", got)
        self.assertIn("[not.a.header]", got)
        self.assertIn("[[exemptions.libc-other]]", got)
        self.assertNotIn("[[exemptions.libc]]", got)
        self.assertNotIn("[[audits.libc]]", got)
        self.assertNotIn("[[audits.google.audits.libc]]", got)
        self.assertNotIn("[[trusted.libc]]", got)


class AgentCriteriaIsolation(unittest.TestCase):
    def test_agent_criteria_do_not_imply_builtin_human_criteria(self):
        repo_root = Path(__file__).resolve().parent.parent
        audits = tomllib.loads((repo_root / "supply-chain" / "audits.toml").read_text())
        builtins = {"safe-to-deploy", "safe-to-run"}
        for name, entry in audits.get("criteria", {}).items():
            implies = entry.get("implies", [])
            if isinstance(implies, str):
                implies = [implies]
            self.assertEqual(
                set(implies) & builtins,
                set(),
                f"custom criterion {name} must stay separate from built-in {builtins}",
            )
        for crate, entries in audits.get("audits", {}).items():
            for entry in entries:
                criteria = entry.get("criteria", [])
                if isinstance(criteria, str):
                    criteria = [criteria]
                self.assertEqual(
                    set(criteria) & builtins,
                    set(),
                    f"local agent-authored audit for {crate} must use a custom agent-* criterion",
                )


if __name__ == "__main__":
    unittest.main()
