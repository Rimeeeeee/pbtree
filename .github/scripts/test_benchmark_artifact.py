"""The privileged publisher reads JSON from an artifact without extracting files."""

import io
import subprocess
import sys
import unittest
import zipfile
from pathlib import Path


class ArtifactReaderTests(unittest.TestCase):
    def read_archive(self, files):
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for name, data in files:
                output.writestr(name, data)
        return subprocess.run(
            [sys.executable, str(Path(__file__).with_name("read-benchmark-artifact.py"))],
            input=archive.getvalue(), capture_output=True,
        )

    def test_reads_only_expected_json(self):
        result = self.read_archive([("results.json", b'{"schema":1}')])
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b'{"schema":1}')

    def test_rejects_extra_paths_duplicates_and_oversized_content(self):
        for files in [
            [("../results.json", b"{}")],
            [("results.json", b"{}"), ("script.sh", b"exit 0")],
            [("results.json", b"{}"), ("results.json", b"{}")],
            [("results.json", b" " * (1024 * 1024 + 1))],
        ]:
            with self.subTest(names=[name for name, _ in files]):
                self.assertNotEqual(self.read_archive(files).returncode, 0)


if __name__ == "__main__":
    unittest.main()
