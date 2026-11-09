"""Resolve userspace leaf samples using the exact frozen ELF and linker map."""

from bisect import bisect_right
from collections import Counter
import hashlib
from pathlib import Path
import re
import struct

NORMAL_SHA256 = "a4fdf8e9669ea0338f1145dbd117eee5cb592209b4b8b48c01ff2fd4bd01f6da"
MAP_SHA256 = "ed5cac74ea4074fb853d0fb242c3609b86a7c2fd79e324b027f450c5a17e1e28"
HEADER = re.compile(r"^\s*(.*?)\s+(\d+)/(\d+)\s+\[(\d+)\]\s+([\d.]+):\s+(\d+)\s+cpu-clock:u:\s*$")
FRAME = re.compile(r"^\s+([0-9a-f]+)\s+(.+?)\s+\((.+)\)\s*$")
DSOFF = re.compile(r"^(.*)\+0x([0-9a-f]+)$")
MAP_ROW = re.compile(r"^\s*([0-9a-f]+)\s+([0-9a-f]+)\s+([0-9a-f]+)\s+(\d+)( +)(.*)$")
HEX = r"((?:0x)?[0-9a-f]+)"
MMAP = re.compile(r"PERF_RECORD_MMAP2?\s+(\d+)/(\d+):\s+\[" + HEX + r"\(" + HEX + r"\)\s+@\s+" + HEX + r"(?:\s+[^\]]+)?\]:\s+(\S+)\s+(.+)$")


def digest(path):
    with Path(path).open("rb") as data:
        return hashlib.file_digest(data, "sha256").hexdigest()


class Symbols:
    def __init__(self, binary, link_map):
        self.binary = Path(binary).resolve(strict=True)
        assert digest(self.binary) == NORMAL_SHA256
        assert digest(link_map) == MAP_SHA256
        with self.binary.open("rb") as data:
            header = data.read(64)
            assert header[:6] == b"\x7fELF\x02\x01"
            assert struct.unpack_from("<HH", header, 16) == (3, 62)
            offset = struct.unpack_from("<Q", header, 40)[0]
            size, count, names_index = struct.unpack_from("<HHH", header, 58)
            assert size == 64 and names_index < count
            data.seek(offset)
            sections = [struct.unpack("<IIQQQQIIQQ", data.read(64)) for _ in range(count)]
            names = sections[names_index]
            data.seek(names[4])
            strings = data.read(names[5])
            text = next(section for section in sections
                        if strings[section[0]:].split(b"\0", 1)[0] == b".text")
        self.start, self.file_offset, self.size = text[3:6]
        rows = []
        active = seen = False
        zero = 0
        with Path(link_map).open() as data:
            for line in data:
                match = MAP_ROW.match(line.rstrip("\n"))
                if not match:
                    continue
                address, _, length, _, spaces, name = match.groups()
                address, length = int(address, 16), int(length, 16)
                if len(spaces) == 1:
                    active = name == ".text"
                    if active:
                        assert not seen and (address, length) == (self.start, self.size)
                        seen = True
                    continue
                if not active or len(spaces) < 16:
                    continue
                assert self.start <= address <= address + length <= self.start + self.size
                if length:
                    rows.append((address, address + length, name))
                else:
                    zero += 1
        assert seen and len(rows) == 205421 and zero == 107
        rows.sort()
        self.rows = rows
        self.starts = [row[0] for row in rows]
        self.maximum_end = []
        maximum = self.start
        covered = 0
        for begin, end, _ in rows:
            if end > maximum:
                covered += end - max(begin, maximum)
                maximum = end
            self.maximum_end.append(maximum)
        self.proof = {
            "binary_sha256": NORMAL_SHA256, "map_sha256": MAP_SHA256,
            "text_vma": self.start, "text_file_offset": self.file_offset,
            "text_bytes": self.size, "positive_size_symbols": len(rows),
            "zero_size_symbols_not_extended": zero, "covered_bytes": covered,
            "unknown_text_bytes": self.size - covered,
        }

    def lookup(self, file_offset):
        if not self.file_offset <= file_offset < self.file_offset + self.size:
            return []
        address = self.start + file_offset - self.file_offset
        index = bisect_right(self.starts, address) - 1
        names = []
        # Preserve every overlapping alias. Never extend a symbol into padding.
        while index >= 0 and self.maximum_end[index] > address:
            begin, end, name = self.rows[index]
            if begin <= address < end:
                names.append(name)
            index -= 1
        return sorted(set(names))


def decode(script):
    samples, metadata = [], []
    current = None
    for line in script.splitlines():
        match = HEADER.match(line)
        if match:
            comm, pid, tid, cpu, timestamp, period = match.groups()
            current = {"comm": comm, "pid": int(pid), "tid": int(tid),
                       "cpu": int(cpu), "timestamp": timestamp,
                       "period": int(period), "frames": []}
            assert current["period"] > 0
            samples.append(current)
            continue
        match = FRAME.match(line)
        if match and current is not None:
            ip, symbol, dso = match.groups()
            offset = DSOFF.match(dso)
            frame = {"ip": ip, "symbol": symbol, "dso": dso}
            if offset:
                frame.update(dso_path=offset[1], file_offset=int(offset[2], 16))
            current["frames"].append(frame)
        elif line.strip():
            if "cpu-clock:u:" in line:
                raise ValueError(f"unparsed userspace sample: {line}")
            metadata.append(line)
            current = None
        else:
            current = None
    return samples, metadata


def classify(samples, metadata, symbols):
    mappings = []
    for line in metadata:
        match = MMAP.search(line)
        if match:
            pid, tid, start, length, offset, permissions, filename = match.groups()
            mappings.append({"pid": int(pid), "tid": int(tid),
                             "start": int(start, 16), "length": int(length, 16),
                             "file_offset": int(offset, 16), "permissions": permissions,
                             "filename": filename})
        elif "PERF_RECORD_MMAP" in line and str(symbols.binary) in line:
            raise ValueError(f"unparsed mapping for normal executable: {line}")
    comm_weights, dso_weights, categories, functions = (Counter() for _ in range(4))
    mapped_samples = crosschecked_samples = 0
    for sample in samples:
        period = sample["period"]
        comm_weights[sample["comm"]] += period
        frames = sample["frames"]
        if not frames:
            sample["leaf_category"] = "missing-frame"
            categories["missing-frame"] += period
            dso_weights["[unknown]"] += period
            continue
        leaf = frames[0]
        filename = leaf.get("dso_path", leaf["dso"])
        dso_weights[filename] += period
        normal = filename == str(symbols.binary)
        if normal:
            assert "file_offset" in leaf, "normal ELF sample lacks dsoff"
            offset, ip = leaf["file_offset"], int(leaf["ip"], 16)
            # Perf owns fork/exec map state. Cross-check its reported file offset
            # against the raw mapping record without guessing an ASLR base.
            candidates = [m for m in mappings if m["pid"] == sample["pid"]
                          and m["filename"] == filename and "x" in m["permissions"]
                          and m["start"] <= ip < m["start"] + m["length"]]
            assert any(ip - m["start"] + m["file_offset"] == offset for m in candidates), leaf
            crosschecked_samples += 1
            names = symbols.lookup(offset)
            leaf["normal_symbols"] = names
            category = "normal-resolved" if names else "normal-unresolved"
            if names:
                functions[" | ".join(names)] += period
                mapped_samples += 1
        else:
            category = "other-dso" if "dso_path" in leaf else "unknown-dso"
        sample["leaf_category"] = category
        categories[category] += period
    total = sum(sample["period"] for sample in samples)
    assert sum(categories.values()) == sum(dso_weights.values()) == sum(comm_weights.values()) == total
    return {"samples": len(samples), "total_period_weight": total,
            "normal_mapped_samples": mapped_samples,
            "normal_mmap_crosschecked_samples": crosschecked_samples,
            "mapping_records": len(mappings), "comm_weights": comm_weights,
            "dso_weights": dso_weights, "leaf_categories": categories,
            "normal_function_weights": functions,
            "scope": "Leaf userspace CPU sample weights only; no inline/caller or wall-time attribution"}
