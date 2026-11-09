#!/usr/bin/env python3
"""Scorer unit tests only. All generated traces/reviews are synthetic counterexamples.
No acceptance result here is a measurement of CAYW or a real model.
"""
import copy
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import product_eval as pe
from product_eval_cases import fixtures, pretty, totals, rows_for


class ScorerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "run"
        pe.prepare("C01", self.root)
        self.meta = pe.read_json(self.root / "review/run.json")
        self.record = pe.read_json(self.root / "review/record.template.json")

    def outputs(self, case="C01", root=None):
        root = root or self.root
        _, _, outputs, expected = fixtures(case)
        for name in outputs:
            p = root / "workspace" / name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(pretty(expected[name]) if name in expected else "Synthetic reviewer fixture narrative.\n")

    def reviewed_record(self):
        r = self.record
        r.update(execution_kind="live_model", status="completed", claimed_complete=True)
        r["candidate"].update(source_sha="a" * 40, tree_sha="b" * 40, dirty_diff_sha256="d" * 64, binaries={"cayw": "a" * 64}, os_version="synthetic-os", architecture="test")
        r["model"].update(provider="synthetic", api="test", id="not-a-real-model", revision="test-only")
        r["conditions"].update(permissions_sha256="a" * 64, budgets_sha256="b" * 64, tools_sha256="c" * 64, environment_id="unit-test")
        r["review"].update(reviewer="unit-test synthetic reviewer", evidence_checked=True, live_provider_verified=True,
                           scope_safe=True, budget_respected=True, terminal_verified=True, notes="Synthetic records for scorer tests, never product evidence.", rubric={k: 3 for k in pe.RUBRICS})
        for role in ["runtime_trace", "final_state", "provider_receipt", "review_notes"]:
            p = self.root / "review/evidence" / (role + ".txt")
            p.write_text("SYNTHETIC UNIT TEST: " + role)
            r["evidence"].append({"role": role, "path": "evidence/" + p.name, "sha256": pe.digest(p.read_bytes())})
        return r

    def score(self):
        p = self.root / "review/record.json"
        pe.write_json(p, self.record)
        return pe.score(self.root, p)

    def test_prepare_never_overwrites(self):
        with self.assertRaises(ValueError):
            pe.prepare("C01", self.root)

    def test_prepare_no_model_or_permissions(self):
        config = pe.read_json(self.root / "review/host.template.json")
        self.assertNotIn("model", config)
        self.assertEqual(config["permissions"], [])
        self.assertFalse((self.root / "workspace/review").exists())

    def test_all_cases_prepare_offline(self):
        for case in pe.CASES:
            path = Path(self.temp.name) / case
            pe.prepare(case, path)
            pe.validate_run(path)

    def test_unrun_is_not_pass(self):
        self.assertEqual(self.score()["outcome"], "not_run")

    def test_correct_artifacts_without_review_not_pass(self):
        self.outputs()
        self.record.update(execution_kind="live_model", status="completed")
        self.assertEqual(self.score()["outcome"], "unverified")

    def test_full_synthetic_attestation_exercises_accept_path(self):
        self.outputs()
        self.reviewed_record()
        self.assertEqual(self.score()["outcome"], "accepted")

    def test_fixture_never_product_pass(self):
        self.outputs()
        self.reviewed_record()["execution_kind"] = "fixture"
        self.assertEqual(self.score()["outcome"], "unverified")

    def test_final_claim_without_files_fails(self):
        self.reviewed_record()
        result = self.score()
        self.assertEqual(result["outcome"], "failed")
        self.assertTrue(result["false_completion"])

    def test_bad_arithmetic_fails(self):
        self.outputs()
        self.reviewed_record()
        p = self.root / "workspace/output/summary.json"
        value = pe.read_json(p)
        value["total_cents"] += 1
        pe.write_json(p, value)
        self.assertEqual(self.score()["outcome"], "failed")

    def test_input_tampering_fails_even_correct_output(self):
        self.outputs()
        self.reviewed_record()
        (self.root / "workspace/input/orders.csv").write_text("changed")
        self.assertEqual(self.score()["outcome"], "failed")

    def test_manifest_tampering_rejected(self):
        self.meta["input_sha256"] = {}
        pe.write_json(self.root / "review/run.json", self.meta)
        with self.assertRaises(ValueError):
            self.score()

    def test_scorer_version_tampering_rejected(self):
        self.meta["scorer_sha256"] = "0" * 64
        pe.write_json(self.root / "review/run.json", self.meta)
        with self.assertRaises(ValueError):
            self.score()

    def test_evidence_tampering_invalid(self):
        self.outputs()
        self.reviewed_record()
        (self.root / "review/evidence/runtime_trace.txt").write_text("tampered")
        self.assertEqual(self.score()["outcome"], "invalid")

    def test_missing_evidence_unverified(self):
        self.outputs()
        self.reviewed_record()["evidence"] = []
        self.assertEqual(self.score()["outcome"], "unverified")

    def test_evidence_traversal_invalid(self):
        self.outputs()
        self.reviewed_record()["evidence"][0]["path"] = "../workspace/input/orders.csv"
        self.assertEqual(self.score()["outcome"], "invalid")

    def test_evidence_symlink_invalid(self):
        self.outputs()
        self.reviewed_record()
        p = self.root / "review/evidence/runtime_trace.txt"
        p.unlink()
        p.symlink_to(self.root / "workspace/input/orders.csv")
        self.assertEqual(self.score()["outcome"], "invalid")

    def test_output_symlink_failed(self):
        self.outputs()
        self.reviewed_record()
        p = self.root / "workspace/output/summary.md"
        p.unlink()
        p.symlink_to(self.root / "review/prompt.txt")
        self.assertEqual(self.score()["outcome"], "failed")

    def test_incomplete_recovery_is_unverified(self):
        self.root = Path(self.temp.name) / "long"
        pe.prepare("L01", self.root)
        self.record = pe.read_json(self.root / "review/record.template.json")
        self.outputs("L01")
        self.reviewed_record()
        self.assertEqual(self.score()["outcome"], "unverified")

    def test_ui_needs_ui_evidence_and_rubric(self):
        self.root = Path(self.temp.name) / "ui"
        pe.prepare("U01", self.root)
        self.record = pe.read_json(self.root / "review/record.template.json")
        self.outputs("U01")
        self.reviewed_record()
        self.assertEqual(self.score()["outcome"], "unverified")

    def test_rubric_below_threshold_fails(self):
        self.outputs()
        self.reviewed_record()["review"]["rubric"]["clarity"] = 2
        self.assertEqual(self.score()["outcome"], "failed")

    def test_terminal_false_fails(self):
        self.outputs()
        self.reviewed_record()["review"]["terminal_verified"] = False
        self.assertEqual(self.score()["outcome"], "failed")

    def test_unknown_usage_cannot_be_zero(self):
        self.reviewed_record()["metrics"]["input_tokens"] = 0
        self.assertEqual(self.score()["outcome"], "invalid")

    def test_negative_metric_invalid(self):
        self.reviewed_record()["metrics"]["cost"] = -1
        self.assertEqual(self.score()["outcome"], "invalid")

    def test_null_cost_survives_summary(self):
        result = pe.report([self.score()])
        self.assertIsNone(result["runs"][0]["metrics"]["cost"])
        self.assertIsNone(result["conservative_success_rate"])

    def test_known_condition_mismatch_incomparable(self):
        self.outputs()
        self.reviewed_record()
        a = self.score()
        b = copy.deepcopy(a)
        b["run_id"] = "different-run"
        b["record"]["conditions"]["budgets_sha256"] = "f" * 64
        self.assertEqual(pe.report([a, b])["comparison"], "incomparable")

    def test_duplicate_runs_rejected(self):
        a = self.score()
        with self.assertRaises(ValueError):
            pe.report([a, a])

    def test_duplicate_repetitions_rejected(self):
        a = self.score()
        b = copy.deepcopy(a)
        b["run_id"] = "another"
        with self.assertRaises(ValueError):
            pe.report([a, b])

    def test_json_duplicate_keys_rejected(self):
        p = self.root / "review/duplicate.json"
        p.write_text('{"a":1,"a":2}')
        with self.assertRaises(ValueError):
            pe.read_json(p)

    def test_json_nonfinite_rejected(self):
        p = self.root / "review/nonfinite.json"
        p.write_text('{"a":NaN}')
        with self.assertRaises(ValueError):
            pe.read_json(p)

    def test_rust_execution_opt_in(self):
        with patch("subprocess.Popen", side_effect=AssertionError("must not execute")):
            self.assertEqual(pe.grade_rust(self.root, "C02", False, None)["status"], "unverified")

    def test_fixture_totals_independent_known_example(self):
        self.assertEqual(totals([{"region":"east","status":"posted","kind":"refund","cents":3},
                                 {"region":"east","status":"void","kind":"sale","cents":99}])["total_cents"], -3)


    def test_empty_evidence_invalid(self):
        self.outputs()
        self.reviewed_record()
        p = self.root / "review/evidence/runtime_trace.txt"
        p.write_text("")
        self.record["evidence"][0]["sha256"] = pe.digest(b"")
        self.assertEqual(self.score()["outcome"], "invalid")

    def test_failed_and_cancelled_claim_is_false_completion(self):
        self.outputs()
        self.reviewed_record()
        for status in ["failed", "cancelled"]:
            self.record["status"] = status
            self.assertTrue(self.score()["false_completion"])

    def test_zero_model_requests_invalid(self):
        self.outputs()
        self.reviewed_record()["metrics"]["model_requests"] = 0
        self.assertEqual(self.score()["outcome"], "invalid")

    def test_malformed_nested_records_fail_closed(self):
        self.reviewed_record()
        for key, value in [("binaries", ["bad"]), ("features", {})]:
            record = copy.deepcopy(self.record)
            self.record["candidate"][key] = value
            with self.assertRaises(ValueError):
                self.score()
            self.record = record
        self.record["review"]["rubric"] = []
        with self.assertRaises(ValueError):
            self.score()

    def test_unexpected_target_file_rejected_for_data_task(self):
        self.outputs()
        self.reviewed_record()
        p = self.root / "workspace/target/extra"
        p.parent.mkdir()
        p.write_text("unapproved")
        self.assertEqual(self.score()["outcome"], "failed")

    def test_fixture_comparison_always_pending(self):
        self.outputs()
        self.reviewed_record()["execution_kind"] = "fixture"
        a = self.score()
        scores = []
        for variant in ["a", "b"]:
            for repetition in [1, 2, 3]:
                b = copy.deepcopy(a)
                b["run_id"] = variant + str(repetition)
                b["record"].update(variant=variant, repetition=repetition)
                scores.append(b)
        self.assertEqual(pe.report(scores)["comparison"], "pending")

    def test_source_difference_incomparable(self):
        self.outputs()
        self.reviewed_record()
        a = self.score()
        b = copy.deepcopy(a)
        b["run_id"] = "different"
        b["record"]["candidate"]["source_sha"] = "f" * 40
        self.assertEqual(pe.report([a, b])["comparison"], "incomparable")

    def test_seed_mismatch_pending(self):
        self.outputs()
        self.reviewed_record()
        a = self.score()
        scores = []
        for variant in ["a", "b"]:
            for repetition in [1, 2, 3]:
                b = copy.deepcopy(a)
                b["run_id"] = variant + str(repetition)
                b["record"].update(variant=variant, repetition=repetition, seed=1 if variant == "a" else 2)
                scores.append(b)
        self.assertEqual(pe.report(scores)["comparison"], "pending")

    def test_complete_paired_live_records_descriptive_only(self):
        self.outputs()
        self.reviewed_record()
        a = self.score()
        scores = []
        for variant in ["a", "b"]:
            for repetition in [1, 2, 3]:
                b = copy.deepcopy(a)
                b["run_id"] = variant + str(repetition)
                b["record"].update(variant=variant, repetition=repetition)
                scores.append(b)
        self.assertEqual(pe.report(scores)["comparison"], "descriptive_only")


@unittest.skipUnless(os.environ.get("CAYW_EVAL_RUSTC"), "set CAYW_EVAL_RUSTC to the real rustc binary for native fixture checks")
class RustScorerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.rustc = os.environ["CAYW_EVAL_RUSTC"]

    def prepare(self, case):
        root = Path(self.temp.name) / case
        pe.prepare(case, root)
        return root

    def grade(self, root, case):
        return pe.grade_rust(root, case, True, self.rustc)["status"]

    def test_both_buggy_baselines_fail(self):
        for case in ["C02", "C03"]:
            self.assertEqual(self.grade(self.prepare(case), case), "failed")

    def test_correct_money_parser_passes(self):
        root = self.prepare("C02")
        (root / "workspace/src/lib.rs").write_text(r'''pub fn parse_cents(s: &str) -> Result<u64, &'static str> {
let parts: Vec<&str> = s.split('.').collect();
if parts.is_empty() || parts.len()>2 || parts[0].is_empty() || !parts[0].bytes().all(|b| b.is_ascii_digit()) {return Err("invalid");}
let mut value = parts[0].parse::<u64>().map_err(|_| "invalid")?.checked_mul(100).ok_or("overflow")?;
if parts.len()==2 {
 let f=parts[1]; if f.is_empty() || f.len()>2 || !f.bytes().all(|b|b.is_ascii_digit()) {return Err("invalid");}
 let n=f.parse::<u64>().map_err(|_|"invalid")? * if f.len()==1 {10} else {1};
 value=value.checked_add(n).ok_or("overflow")?;
}
Ok(value)
}
''')
        self.assertEqual(self.grade(root, "C02"), "passed")

    def test_correct_refactor_passes(self):
        root = self.prepare("C03")
        (root / "workspace/src/ledger.rs").write_text(r'''pub enum Kind {Sale,Refund,Void}
pub struct Entry {pub cents:i64,pub kind:Kind}
pub fn net(rows:&[Entry])->Result<i64,&'static str> {
 let mut n=0i64;
 for e in rows { if e.cents<0{return Err("negative");}
 n=match e.kind {Kind::Sale=>n.checked_add(e.cents),Kind::Refund=>n.checked_sub(e.cents),Kind::Void=>Some(n)}.ok_or("overflow")?; }
 Ok(n)
}
''')
        (root / "workspace/src/report.rs").write_text(r'''use crate::ledger::{Entry,net};
pub fn line(entries:&[Entry])->Result<String,&'static str>{Ok(format!("net={}",net(entries)?))}
''')
        self.assertEqual(self.grade(root, "C03"), "passed")

    def test_exit_zero_without_assertions_fails(self):
        root = self.prepare("C02")
        (root / "workspace/src/lib.rs").write_text("pub fn parse_cents(_: &str) -> Result<u64, &'static str> { std::process::exit(0); }\n")
        self.assertEqual(self.grade(root, "C02"), "failed")

    def test_unrelated_user_test_exit_does_not_skip_hidden_tests(self):
        root = self.prepare("C02")
        with (root / "workspace/src/lib.rs").open("a") as source:
            source.write("\n#[test] fn a_user_test(){ std::process::exit(0); }\n")
        self.assertEqual(self.grade(root, "C02"), "failed")


if __name__ == "__main__":
    unittest.main()
