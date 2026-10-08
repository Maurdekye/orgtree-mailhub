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
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import unittest
import unittest.mock
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

    # ── the MH02 preparation register ───────────────────────────────────────
    # Four new controls, so the suite's denominator moves from 32 to 36. The
    # register exists to make an unregistered source file a failure; these
    # prove it still does that after being widened.

    def test_every_mh02_preparation_path_is_registered_exactly(self):
        """The ten preparation paths are accepted, and each is spelled out."""
        self.assertEqual(10, len(inventory.MH02_ADDITIONS))
        self.assertTrue(inventory.MH02_ADDITIONS <= inventory.ADDITIONS)
        for path in inventory.MH02_ADDITIONS:
            self.assertFalse(path.endswith("/") or "*" in path,
                             "%r is a wildcard, not an exact path" % path)
        self.assertEqual([], inventory.check(self.frozen, self.files,
                                             [*self.files, *inventory.MH02_ADDITIONS]))

    def test_an_unregistered_preparation_file_is_rejected(self):
        """A new file inside the crate that nobody registered must be refused,
        so `native/` never becomes an unreviewed dumping ground."""
        for intruder in ("native/mailhub-protocol/src/transport.rs",
                         "native/mailhub-protocol/build.rs",
                         "native/mailhub-runtime/src/main.rs"):
            self.assertTrue(
                inventory.check(self.frozen, self.files,
                                [*self.files, *inventory.MH02_ADDITIONS, intruder]),
                "%r was admitted without being registered" % intruder)

    def test_each_registered_preparation_path_is_load_bearing(self):
        """Dropping any single entry from the register makes exactly that path
        refused -- a prefix rule would make the entries interchangeable."""
        for path in sorted(inventory.MH02_ADDITIONS):
            narrowed = inventory.ADDITIONS - {path}
            with unittest.mock.patch.object(inventory, "ADDITIONS", narrowed):
                self.assertTrue(
                    inventory.check(self.frozen, self.files,
                                    [*self.files, *inventory.MH02_ADDITIONS]),
                    "%r stayed admitted after being removed from the register" % path)

    def test_the_preparation_paths_add_no_product_runtime_source(self):
        """Registering an artifact is not the same as censusing product source.
        None of the ten may appear in the frozen source denominator, and the
        denominator itself must not move."""
        censused = {row["path"] for row in self.frozen["files"]}
        self.assertEqual(set(), censused & inventory.MH02_ADDITIONS)
        self.assertEqual(26, len(censused))
        self.assertEqual(inventory.BASE, self.frozen["source_commit"])

    # ── the authorized-modification register ────────────────────────────────
    # Every file that existed at BASE is pinned by content, and until f2 there
    # was no way to say that a change to one was MEANT to be there: the
    # one-line Docker build-context fix was indistinguishable from drift. The
    # register names one exact content per path. These controls prove it is a
    # named-content exception and not a hole -- the acceptance below is worth
    # nothing unless the refusals still fire.

    def _holding(self, path, raw):
        """The pinned BASE sources with one file's bytes replaced."""
        current = dict(self.files)
        current[path] = raw
        return current

    @property
    def _authorized_dockerignore(self):
        """Spelled out here rather than read back from the register, so this
        file states independently what the one authorized content IS."""
        return self.files[".dockerignore"] + b"native/**/target/\n"

    def test_the_modification_register_is_exactly_spelled_out(self):
        """One entry, an exact path, and a content named by hash AND size."""
        self.assertEqual(1, len(inventory.MODIFICATIONS))
        for path, named in inventory.MODIFICATIONS.items():
            self.assertFalse(path.endswith("/") or "*" in path,
                             "%r is a wildcard, not an exact path" % path)
            self.assertEqual({"sha256", "bytes"}, set(named))
            self.assertEqual(64, len(named["sha256"]))
            self.assertEqual(named["sha256"], named["sha256"].lower().strip())

    def test_the_authorized_dockerignore_is_accepted(self):
        """The registered content passes, and the register names that exact
        content rather than merely some longer file."""
        named = inventory.MODIFICATIONS[".dockerignore"]
        self.assertEqual(named["sha256"], hashlib.sha256(self._authorized_dockerignore).hexdigest())
        self.assertEqual(named["bytes"], len(self._authorized_dockerignore))
        self.assertEqual([], inventory.check(
            self.frozen, self._holding(".dockerignore", self._authorized_dockerignore),
            list(self.files)))

    def test_the_substitution_itself_only_accepts_the_named_content(self):
        """The guard asserted directly rather than through a refusal.

        check() refuses unauthorized content twice over -- once because the
        rebuild disagrees with the freeze and once by naming the registered
        file specifically -- so no outcome-level test can tell whether
        authorized() is guarding anything at all. This one looks at what it
        actually substitutes."""
        third = self.files[".dockerignore"] + b"native/**/target\n"
        self.assertEqual(self.frozen,
                         inventory.authorized(self.frozen, self._holding(".dockerignore", third)),
                         "unauthorized bytes were substituted into the freeze")
        substituted = inventory.authorized(
            self.frozen, self._holding(".dockerignore", self._authorized_dockerignore))
        named = inventory.MODIFICATIONS[".dockerignore"]
        record = {r["path"]: r for r in substituted["files"]}[".dockerignore"]
        self.assertEqual((named["sha256"], named["bytes"]), (record["sha256"], record["bytes"]))
        self.assertEqual([r for r in self.frozen["files"] if r["path"] != ".dockerignore"],
                         [r for r in substituted["files"] if r["path"] != ".dockerignore"],
                         "substituting one record disturbed another")
        self.assertEqual({k: v for k, v in self.frozen.items() if k != "files"},
                         {k: v for k, v in substituted.items() if k != "files"},
                         "the substitution reached beyond the file records")

    def test_the_frozen_dockerignore_is_still_accepted(self):
        """Registering a modification must not make the ORIGINAL bytes fail:
        censusing pinned BASE blobs is how every replay of this census works,
        and the register must leave the freeze alone when it sees them."""
        self.assertEqual(self.frozen, inventory.authorized(self.frozen, self.files))
        self.assertEqual([], inventory.check(self.frozen, self.files, list(self.files)))

    def test_any_other_dockerignore_content_is_rejected(self):
        """One named content, not a licence to edit the file. A near miss is
        still a miss -- these differ from the authorization by one character,
        one line, or one repetition."""
        base = self.files[".dockerignore"]
        for label, raw in (
                ("the pattern without its trailing slash", base + b"native/**/target\n"),
                ("a shallower pattern", base + b"native/*/target/\n"),
                ("the pattern plus one more line", base + b"native/**/target/\nmailhub/\n"),
                ("the pattern with an existing line dropped",
                 base.replace(b"hubtool.py\n", b"") + b"native/**/target/\n"),
                ("the pattern twice", base + b"native/**/target/\nnative/**/target/\n"),
                ("the whole file replaced by the pattern", b"native/**/target/\n")):
            self.assertTrue(
                inventory.check(self.frozen, self._holding(".dockerignore", raw), list(self.files)),
                "%s was admitted" % label)

    def test_a_registered_file_holding_a_third_content_is_named_in_the_error(self):
        """Drift in a file somebody WAS allowed to change reads differently
        from drift in one nobody could touch, so the census has to SAY which
        of the two it found rather than reporting both the same way."""
        errors = inventory.check(
            self.frozen,
            self._holding(".dockerignore", self.files[".dockerignore"] + b"native/**/target\n"),
            list(self.files))
        self.assertTrue(
            any(e.startswith("registered files holding neither the frozen nor the authorized bytes")
                and ".dockerignore" in e for e in errors), errors)

    def test_an_unregistered_base_file_modification_is_rejected(self):
        """Per path, not per family and not per kind. .gitignore and
        .gitattributes sit in the same repository-metadata family as
        .dockerignore and are still refused; so is every packaging file."""
        for path in (".gitignore", ".gitattributes", "LICENSE", "Dockerfile",
                     "compose.yaml", "requirements.txt", ".env.example"):
            self.assertNotIn(path, inventory.MODIFICATIONS)
            self.assertTrue(
                inventory.check(self.frozen,
                                self._holding(path, self.files[path] + b"\n# appended by nobody\n"),
                                list(self.files)),
                "%r drifted without being registered" % path)

    def test_the_register_may_only_name_frozen_paths(self):
        """A typo, or a path that never existed at BASE, has to be a failure
        rather than an inert line nobody notices."""
        for stray in ("dockerignore", ".dockerignore ", "native/mailhub-protocol/Cargo.toml"):
            with unittest.mock.patch.object(
                    inventory, "MODIFICATIONS", {stray: {"sha256": "0" * 64, "bytes": 0}}):
                self.assertTrue(inventory.check(self.frozen, self.files, list(self.files)),
                                "%r was accepted as a registered modification" % stray)

    def test_the_register_cannot_wave_through_a_contract_change(self):
        """It substitutes a file's hash and size and nothing else. Authorize a
        modified mailhub/app.py, hand over exactly those bytes, and the census
        must STILL refuse -- the routes and refusals are rebuilt from the bytes
        in hand, so a registration can never buy a behaviour change."""
        smuggled = self.files["mailhub/app.py"] + (
            b'\n\n@app.get("/smuggled")\ndef smuggled():\n    return None\n')
        with unittest.mock.patch.object(
                inventory, "MODIFICATIONS",
                {"mailhub/app.py": {"sha256": hashlib.sha256(smuggled).hexdigest(),
                                    "bytes": len(smuggled)}}):
            self.assertTrue(inventory.check(self.frozen,
                                            self._holding("mailhub/app.py", smuggled),
                                            list(self.files)))

    def test_each_registered_modification_is_load_bearing(self):
        """Empty the register and the authorized content fails again, so the
        acceptance above comes from the register rather than from the check
        having quietly stopped looking at that file."""
        with unittest.mock.patch.object(inventory, "MODIFICATIONS", {}):
            self.assertTrue(inventory.check(
                self.frozen, self._holding(".dockerignore", self._authorized_dockerignore),
                list(self.files)))

    def test_the_checkout_holds_one_of_the_two_named_contents(self):
        """Asserted against the real checkout rather than against fixtures:
        .dockerignore on disk is either the frozen bytes or the authorized
        bytes, never a third thing. Line endings are normalized the way the
        census itself normalizes them."""
        on_disk = (ROOT / ".dockerignore").read_bytes().replace(b"\r\n", b"\n")
        frozen = {r["path"]: r["sha256"] for r in self.frozen["files"]}[".dockerignore"]
        self.assertIn(hashlib.sha256(on_disk).hexdigest(),
                      {frozen, inventory.MODIFICATIONS[".dockerignore"]["sha256"]})

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


class StateFamilyControls(unittest.TestCase):
    """Reviewer finding F2: a file/function/SQL census is not the registry MH01
    owes. Counting 26 files says nothing about a memory-only queue, a file that
    carries process ownership, or an artifact written outside the blob root.
    These controls require each of those to stay dispositioned and anchored."""

    @classmethod
    def setUpClass(cls):
        cls.frozen = json.loads(_FROZEN.read_text(encoding="utf-8"))
        cls.files = inventory.source_files()
        cls.fams = cls.frozen["state_families"]["families"]

    def test_every_named_family_is_present_and_anchored(self):
        """Each family resolves to a real line in the pinned source."""
        self.assertEqual([], self.frozen["state_families"]["unresolved"])
        for f in self.fams:
            self.assertTrue(f["lines"], "%s resolved to no source line" % f["family"])
            self.assertFalse(f["missing_source"], f["family"])

    def test_the_families_a_file_census_misses_are_all_recorded(self):
        """The six the reviewer named, by category rather than by name alone: a
        queue, process custody, an output artifact outside the blob root, a
        configuration mutation, a shell-level admission gate and a protocol
        envelope. Losing any one of these in a port loses durable behaviour a
        file count cannot see."""
        got = {f["family"]: f["category"] for f in self.fams}
        for name, category in {
            "receipt-retry-queue": "queue",
            "listener-process-ownership": "process-custody",
            "fetched-attachment-output": "output-artifact",
            "onboarding-settings-mutation": "configuration-state",
            "session-admission-environment": "configuration-source",
            "mcp-jsonrpc-envelope": "protocol-envelope",
        }.items():
            self.assertEqual(category, got.get(name), "missing or miscategorised: %s" % name)

    def test_dropping_a_family_from_the_register_is_rejected(self):
        changed = copy.deepcopy(self.frozen)
        changed["state_families"]["families"] = [
            f for f in changed["state_families"]["families"]
            if f["family"] != "receipt-retry-queue"]
        self.assertTrue(inventory.check(changed, self.files, list(self.files)))

    def test_a_dead_family_anchor_aborts(self):
        """The anchors are RESOLVED, not trusted. If the source moves on, the
        register must fail rather than keep describing code that is gone.

        `_resolve` aborts the whole build rather than returning empty, which is
        the stronger of the two fail-closed shapes: a register that quietly
        listed a family with no lines would still look complete in the JSON."""
        original = inventory.STATE_FAMILIES
        try:
            inventory.STATE_FAMILIES = [
                {**original[0], "marker": "this marker is not in the pinned source"}]
            with self.assertRaises(SystemExit):
                inventory.state_families(self.files)
        finally:
            inventory.STATE_FAMILIES = original

    def test_every_family_carries_a_disposition_and_its_unknowns(self):
        """An unknown that is not written down is an unknown that surfaces in
        MH02 instead. Every family states what a port owes and what was not
        exercised."""
        for f in self.fams:
            self.assertTrue(f["disposition"], f["family"])
            self.assertTrue(f["obligation"], f["family"])
            self.assertIsInstance(f["unknowns"], list)
            self.assertTrue(f["unknowns"], "%s records no unknowns" % f["family"])

    def test_the_listener_lock_release_is_anchored_not_asserted(self):
        """Review round 2: the register claimed the lock was "never removed on
        exit". hubtool.py contradicts that -- listen() removes it in a `finally`
        -- so the freeze was recording a defect the source does not have. The
        corrected claim is anchored at the removal itself, and the family must
        carry BOTH line sets: taking the lock and releasing it."""
        fam = next(f for f in self.frozen["state_families"]["families"]
                   if f["family"] == "listener-process-ownership")
        self.assertNotIn("never removed", fam["loss"].lower(),
                         "the contradicted claim is back in the register")
        self.assertTrue(fam.get("release_lines"),
                        "the release point is unanchored, so the loss claim "
                        "can drift from the source again")
        src = self.files["hubtool.py"].decode("utf-8").splitlines()
        for n in fam["release_lines"]:
            self.assertIn("os.remove(lock)", src[n - 1])
        # The acquisition is still anchored, and the two are distinct points.
        self.assertTrue(fam["lines"])
        self.assertLess(max(fam["lines"]), min(fam["release_lines"]))

        # Round 3: the register also claimed the cleanup was reliable. It is
        # not -- the unlink swallows OSError -- so the "best effort" wording is
        # checked against the SOURCE rather than taken on trust. A port that
        # reads only the prose must not come away believing the lock is always
        # released.
        guard = chr(10).join(l for n in fam["release_lines"] for l in src[n - 1:n + 2])
        self.assertIn("except OSError", guard,
                      "the anchored unlink is not the guarded one this claim "
                      "describes")
        self.assertIn("attempted", fam["loss"].lower(),
                      "the register states the cleanup more strongly than the "
                      "source supports")

    def test_the_unexercised_families_block_conversion(self):
        """MH01 may not arm a listener or speak MCP, so these two are frozen
        from source and NOT exercised. That has to be recorded as blocking, or
        the freeze reads as verification it is not."""
        self.assertEqual(["listener-process-ownership", "mcp-jsonrpc-envelope"],
                         self.frozen["state_families"]["blocking_unknowns"])

    def test_pid_reuse_is_named_as_the_custody_hazard(self):
        """_pid_alive trusts an integer with no start-time check, so a reused
        pid reads as the live holder and refuses the real listener. Naming it is
        what stops a port inheriting it by accident."""
        fam = next(f for f in self.fams if f["family"] == "listener-process-ownership")
        self.assertTrue(any("PID REUSE" in u.upper() for u in fam["unknowns"]))

    def test_the_mcp_envelope_records_that_it_has_no_error_member(self):
        """Every serve() reply is {jsonrpc, id, result}; errors are carried as
        TEXT inside a success result. A port that fixes this to a real JSON-RPC
        error changes every client's observable behaviour."""
        fam = next(f for f in self.fams if f["family"] == "mcp-jsonrpc-envelope")
        self.assertIn("2024-11-05", fam["semantics"])
        self.assertIn("there is no error member", fam["semantics"])


class RefusalEnvelopeControls(unittest.TestCase):
    """Reviewer finding F1, census half: a status code alone does not pin a
    refusal. The detail is extracted from the pinned AST so the wire profile
    asserts the source's own text rather than a transcription of it."""

    @classmethod
    def setUpClass(cls):
        cls.frozen = json.loads(_FROZEN.read_text(encoding="utf-8"))
        cls.files = inventory.source_files()

    def test_every_refusal_status_carries_a_detail(self):
        for route in self.frozen["http_contract"]:
            statuses = sorted({r["status"] for r in route["refusals"]})
            self.assertEqual(route["refusal_statuses"], statuses,
                             "%s %s: refusals and refusal_statuses disagree"
                             % (route["method"], route["path"]))
            for r in route["refusals"]:
                self.assertIsNotNone(r["detail"], "%s %s %s has no detail"
                                     % (route["method"], route["path"], r["status"]))
                self.assertIn(r["detail"]["kind"],
                              {"literal", "concatenation", "template"},
                              "%s %s: unresolvable detail" % (route["path"], r["status"]))

    def test_the_two_401_variants_stay_distinct(self):
        """/api/poll and /api/unregister say 'no valid org credentials in
        X-Org-Auth'; the rest say 'no valid org credentials'. Collapsing them is
        a silent contract change, and a status-only profile cannot see it."""
        by = {}
        for route in self.frozen["http_contract"]:
            for r in route["refusals"]:
                if r["status"] == 401:
                    by.setdefault(r["detail"]["text"], set()).add(route["path"])
        self.assertEqual({"/api/poll", "/api/unregister"},
                         by.get("no valid org credentials in X-Org-Auth", set()))
        self.assertIn("/api/roster", by.get("no valid org credentials", set()))

    def test_changing_a_refusal_detail_is_rejected(self):
        changed = dict(self.files)
        changed["mailhub/app.py"] = changed["mailhub/app.py"].replace(
            b'"malformed slug"', b'"bad slug"')
        self.assertTrue(inventory.check(self.frozen, changed, list(changed)))

    def test_an_f_string_detail_keeps_its_constant_runs(self):
        """A template interpolation is a per-request value a profile must not
        hard-code, but the text around it is contractual."""
        templates = [r["detail"] for route in self.frozen["http_contract"]
                     for r in route["refusals"] if r["detail"]["kind"] == "template"]
        self.assertTrue(templates)
        for t in templates:
            self.assertIn("{}", t["text"])
            self.assertTrue([p for p in t["parts"] if p.strip()],
                            "template %r froze no literal text" % t["text"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
