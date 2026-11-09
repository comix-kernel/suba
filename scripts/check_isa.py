"""Check the actual kernel text against the integer-only task context."""
import re
import subprocess
import sys

output = subprocess.check_output(
    ["llvm-objdump", "-d", "--no-show-raw-insn", "--mattr=+f,+d,+v", sys.argv[1]],
    text=True)
instructions = re.findall(r"^\s*[0-9a-f]+:\s+(\S+)", output, re.MULTILINE)
if not instructions:
    raise SystemExit("kernel text is empty")
for op in instructions:
    if op.startswith(("v", "c.f")) or (op.startswith("f") and not op.startswith("fence")):
        raise SystemExit(f"unsaved floating-point/vector register state required by {op}")
