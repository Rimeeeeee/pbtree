"""Read the one expected JSON file without extracting paths from an untrusted ZIP."""

import io
import sys
import zipfile

with zipfile.ZipFile(io.BytesIO(sys.stdin.buffer.read(5 * 1024 * 1024))) as archive:
    entries = archive.infolist()
    if len(entries) != 1 or entries[0].filename != "results.json" or entries[0].file_size > 1024 * 1024:
        raise ValueError("Invalid benchmark artifact")
    with archive.open(entries[0]) as result:
        data = result.read(1024 * 1024 + 1)
    if len(data) > 1024 * 1024:
        raise ValueError("Benchmark artifact is too large")
    sys.stdout.buffer.write(data)
