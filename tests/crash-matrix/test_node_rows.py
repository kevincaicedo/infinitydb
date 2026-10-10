"""Planted carriers prove the gate rejects missing execution and verdicts."""

from pathlib import Path
import tempfile
import sys
import unittest

sys.dont_write_bytecode = True
import run_node_rows as driver


class NodeRows(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.scratch = tempfile.TemporaryDirectory(prefix="inf-node-receipts-")
        cls.directory = Path(cls.scratch.name)
        cls.binary = cls.directory / "fixtures"
        receipt = Path(__file__).resolve().with_name("receipt.rs")
        source = cls.directory / "fixtures.rs"
        source.write_text(f'#[path = "{receipt}"] mod receipt;\n' + r'''
#[test] fn proved() { assert_eq!(2 + 2, 4); receipt::verified("cut", "survives"); }
#[test] fn empty_body() {}
#[test] fn early_return() { if true { return; } receipt::verified("cut", "survives"); }
#[test] #[ignore] fn ignored() { receipt::verified("cut", "survives"); }
#[test] fn wrong_point() { receipt::verified("another-cut", "survives"); }
#[test] fn wrong_verdict() { receipt::verified("cut", "invented"); }
#[test] fn duplicate() {
    receipt::verified("cut", "survives"); receipt::verified("cut", "survives");
}
#[test] fn failed_after_receipt() { receipt::verified("cut", "survives"); panic!("planted"); }
#[test] fn stale_receipt() { eprintln!("CRASH_MATRIX_VERIFIED\told-token\tcut\tsurvives"); }
''')
        driver.run(["rustc", "--edition=2024", "--test", str(source), "-o", str(cls.binary)])

    @classmethod
    def tearDownClass(cls):
        cls.scratch.cleanup()

    def test_only_executed_proofs_pass(self):
        rows = [{"point": "cut", "expect": "survives"}]
        driver.verify_carrier(self.binary, "proved", rows)
        for function in ("deleted", "empty_body", "early_return", "ignored", "wrong_point",
                         "wrong_verdict", "duplicate", "failed_after_receipt", "stale_receipt"):
            with self.subTest(function=function), self.assertRaises((AssertionError, RuntimeError)):
                driver.verify_carrier(self.binary, function, rows)

    def test_an_invented_row_cannot_borrow_a_passing_test(self):
        with self.assertRaisesRegex((AssertionError, RuntimeError), "unproved node rows"):
            driver.verify_carrier(self.binary, "proved", [{"point": "cut", "expect": "invented"}])

    def test_unexpected_receipts_and_duplicate_rows_are_rejected(self):
        with self.assertRaisesRegex((AssertionError, RuntimeError), "unexpected"):
            driver.verify_carrier(self.binary, "proved", [])
        with tempfile.TemporaryDirectory(prefix="inf-node-matrix-") as name:
            directory = Path(name)
            valid = ('seeds = 1\n[[row]]\ntier = "node"\npoint = "cut"\n'
                     'test = "fixture::target::proved"\nexpect = "survives"\n')
            for matrix in driver.MATRICES:
                (directory / matrix).write_text(valid)
            with self.assertRaisesRegex((AssertionError, RuntimeError), "duplicate node row"):
                driver.load_rows(directory)

    def test_invalid_carriers_and_unexecuted_tiers_are_rejected(self):
        with tempfile.TemporaryDirectory(prefix="inf-node-matrix-") as name:
            directory = Path(name)
            valid = ('seeds = 1\n[[row]]\ntier = "node"\npoint = "cut"\n'
                     'test = "fixture::target::proved"\nexpect = "survives"\n')
            mutations = (
                valid.replace("fixture::target::proved", "tier.rs"),
                valid.replace("survives", ""),
                valid.replace('tier = "node"', 'tier = "unimplemented"'),
                valid.replace('tier = "node"', 'tier = "memfs"'),
                valid.replace("seeds = 1", "seeds = 0"),
                valid + 'platform = "never"\n',
                valid + 'policies = ["always"]\n',
            )
            for mutation in mutations:
                for index, matrix in enumerate(driver.MATRICES):
                    (directory / matrix).write_text(valid.replace('"cut"', f'"cut{index}"'))
                (directory / "m4.toml").write_text(mutation)
                with self.subTest(mutation=mutation), self.assertRaises(RuntimeError):
                    driver.load_rows(directory)


if __name__ == "__main__":
    unittest.main()
