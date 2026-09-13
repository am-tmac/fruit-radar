"""Mutate in-memory source only; prove the baseline contract covers browser selection."""
import importlib.util
import io
import pathlib
import unittest

spec = importlib.util.spec_from_file_location("contract", pathlib.Path(__file__).with_name("query-baseline-contract.py"))
assert spec is not None and spec.loader is not None
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)
original = contract.CURRENT
mutation = original.replace('"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"', '"/Applications/Unexpected.app/Contents/MacOS/Unexpected"', 1)
assert mutation != original
contract.CURRENT = mutation
output = io.StringIO()
result = unittest.TextTestRunner(stream=output).run(unittest.defaultTestLoader.loadTestsFromTestCase(contract.QueryContract))
print(output.getvalue())
assert not result.wasSuccessful(), "find_chromium mutation escaped the baseline contract"
print("PASS: find_chromium mutation rejected without touching production source")
