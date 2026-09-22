"""Fail-closed census controls: omissions, new entries, source drift and no silent refresh.

A census that cannot fail proves nothing, so every control here breaks
something deliberately and requires the checker to NOTICE. The first test is
the only one that asserts the real source is clean; the rest are bad controls.

    <python> tests/test_mh01_inventory.py [-v]

MH01_REPO overrides which checkout is censused, and MH01_INVENTORY which
frozen JSON it is compared against, so the same controls can run from outside
the product tree.
"""
import copy
import importlib.util
import json
import os
from pathlib import Path
import unittest
import warnings

ROOT = Path(os.environ.get("MH01_REPO") or Path(__file__).resolve().parents[1])
_TOOL = Path(os.environ.get("MH01_TOOL") or (ROOT / "tools/mh01_inventory.py"))
spec = importlib.util.spec_from_file_location("mh01_inventory", _TOOL)
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)
inventory.ROOT = ROOT
_FROZEN = Path(os.environ.get("MH01_INVENTORY") or (ROOT / inventory.OUTPUT))


class InventoryControls(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.frozen = json.loads(_FROZEN.read_text(encoding="utf-8"))
        cls.files = inventory.source_files()

    # ── the one positive assertion ──────────────────────────────────────────

    def test_exact_source(self):
        self.assertEqual([], inventory.check(self.frozen, self.files, list(self.files)))

    # ── source denominator ──────────────────────────────────────────────────

    def test_omitted_route_rejected(self):
        changed = copy.deepcopy(self.frozen)
        changed["registrations"].pop()
        self.assertTrue(inventory.check(changed, self.files, list(self.files)))

    def test_missing_source_rejected(self):
        changed = dict(self.files)
        del changed["hubtool.py"]
        self.assertTrue(inventory.check(self.frozen, changed, list(changed)))

    def test_unknown_source_rejected(self):
        self.assertTrue(inventory.check(self.frozen, self.files, [*self.files, "mailhub/new_transport.py"]))

    def test_changed_contract_rejected(self):
        changed = dict(self.files)
        changed["mailhub/app.py"] = changed["mailhub/app.py"].replace(b"BODY_MAX = 20000", b"BODY_MAX = 20001")
        self.assertTrue(inventory.check(self.frozen, changed, list(changed)))

    def test_client_schema_omission_rejected(self):
        changed = copy.deepcopy(self.frozen)
        changed["literal_schemas"] = [s for s in changed["literal_schemas"]
                                      if s["path"] != "hubtool.py"]
        self.assertTrue(inventory.check(changed, self.files, list(self.files)))

    # ── the per-route wire contract ─────────────────────────────────────────

    def test_http_contract_covers_every_explicit_route(self):
        routes = {(r["kind"].upper(), r["argument"]) for r in self.frozen["registrations"]
                  if r["kind"] in {"get", "post"}}
        contract = {(r["method"], r["path"]) for r in self.frozen["http_contract"]}
        self.assertEqual(routes, contract)

    def test_http_contract_omission_rejected(self):
        changed = copy.deepcopy(self.frozen)
        changed["http_contract"].pop()
        self.assertTrue(inventory.check(changed, self.files, list(self.files)))

    def test_refusal_status_drift_rejected(self):
        """Dropping a refusal a route really raises must not pass silently."""
        changed = copy.deepcopy(self.frozen)
        for row in changed["http_contract"]:
            if row["path"] == "/api/register":
                row["refusal_statuses"] = [422]
        self.assertTrue(inventory.check(changed, self.files, list(self.files)))

    def test_operator_surface_is_recorded_as_uncredentialed(self):
        """RULED behaviour, and the reason the public listener exists. If this
        ever reads as credentialed, either the source changed or the extractor
        is lying -- both must stop the freeze."""
        by_path = {r["path"]: r for r in self.frozen["http_contract"]}
        for path in ("/", "/ui/data", "/ui/messages", "/healthz"):
            self.assertEqual("none", by_path[path]["credential_check"], path)
        self.assertEqual("inline-header", by_path["/api/register"]["credential_check"])
        for path in ("/api/send", "/api/poll", "/api/ack", "/api/receipts",
                     "/api/roster", "/api/unregister", "/api/attachments",
                     "/api/attachments/{aid}"):
            self.assertEqual("auth-helper", by_path[path]["credential_check"], path)

    # ── the interpreter-dependency register ─────────────────────────────────

    def test_every_backend_interpreter_witness_is_dispositioned(self):
        self.assertEqual([], self.frozen["python_dependencies"]["uncovered_backend_witnesses"])

    def test_a_new_interpreter_launch_is_caught(self):
        """The control that matters for R10: add an interpreter call to a
        backend file and the census must refuse to call the register complete."""
        changed = dict(self.files)
        changed["mailhub/serve.py"] = changed["mailhub/serve.py"] + b'\n# fallback: python -m mailhub.legacy\n'
        rebuilt = inventory.build(changed)
        self.assertTrue(rebuilt["python_dependencies"]["uncovered_backend_witnesses"],
                        "a new interpreter witness was absorbed without a disposition")

    def test_dropping_a_role_uncovers_its_witnesses(self):
        original = inventory.PYTHON_ROLES
        try:
            inventory.PYTHON_ROLES = [r for r in original
                                      if r["role"] != "hub-container-healthcheck"]
            rebuilt = inventory.build(self.files)
            uncovered = rebuilt["python_dependencies"]["uncovered_backend_witnesses"]
            self.assertTrue(any(w["path"] == "compose.yaml" for w in uncovered),
                            "the compose healthcheck went missing without complaint")
        finally:
            inventory.PYTHON_ROLES = original

    def test_a_dead_role_marker_aborts(self):
        """A role describing source that no longer exists must abort the build
        rather than quietly resolve to nothing."""
        original = inventory.PYTHON_ROLES
        try:
            inventory.PYTHON_ROLES = [*original,
                                      {"role": "phantom", "path": "Dockerfile",
                                       "marker": "no such line anywhere",
                                       "disposition": "must-be-replaced", "obligation": "x"}]
            with self.assertRaises(SystemExit):
                inventory.build(self.files)
        finally:
            inventory.PYTHON_ROLES = original

    def test_healthcheck_and_schema_init_are_both_recorded(self):
        """The two dependencies a text search for 'python' gets wrong: the
        compose healthcheck (a second launch, easy to miss) and schema
        initialization (no interpreter word on its line at all)."""
        roles = {r["role"]: r for r in self.frozen["python_dependencies"]["roles"]}
        self.assertIn("hub-container-healthcheck", roles)
        self.assertEqual("compose.yaml", roles["hub-container-healthcheck"]["path"])
        self.assertIn("hub-schema-initialization", roles)
        self.assertEqual("mailhub/db.py", roles["hub-schema-initialization"]["path"])

    def test_test_tooling_is_not_counted_as_a_backend_dependency(self):
        """The docket rules this explicitly; recording it the other way would
        make R10 chase a dependency that is not one."""
        roles = {r["role"]: r for r in self.frozen["python_dependencies"]["roles"]}
        self.assertEqual("test-tooling-not-a-backend-dependency",
                         roles["docker-verification-tooling"]["disposition"])

    def test_parent_claims_are_marked_unverified(self):
        """The parent's two subprocess launches are somebody else's source. They
        are carried as attributed claims, never as facts this probe checked."""
        for row in self.frozen["python_dependencies"]["external_witnesses"]:
            self.assertFalse(row["verified_here"], row["role"])
            self.assertTrue(row["attributed_to"])

    # ── generator hygiene ───────────────────────────────────────────────────

    def test_build_leaks_no_resources(self):
        """The literal DDL is interpreted in an in-memory SQLite connection. A
        bare `with sqlite3.connect(...)` commits but never CLOSES it, which
        leaked the connection and raised ResourceWarning."""
        with warnings.catch_warnings():
            warnings.simplefilter("error", ResourceWarning)
            inventory.build(self.files)

    def test_summary_separates_verb_witnesses_from_verbs(self):
        """addhub/drophub each appear in two branches, so 11 comparison sites
        represent 9 distinct commands. Reporting the witness count as a verb
        count overstates the CLI surface."""
        s = inventory.summary(self.frozen)
        self.assertEqual(11, s["cli_verb_witnesses"])
        self.assertEqual(9, s["cli_verbs"])
        self.assertEqual(9, len({r["value"] for r in self.frozen["cli_dispatch"]}))


if __name__ == "__main__":
    unittest.main(verbosity=2)
