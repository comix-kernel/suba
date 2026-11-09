"""Drive the real UART of the self-test and production images, with bounded waits."""
import argparse
import os
import re
import select
import subprocess
import time

parser = argparse.ArgumentParser()
parser.add_argument("--stage", type=int, required=True, choices=range(1, 7))
parser.add_argument("--shell", action="store_true")
parser.add_argument("--no-trace", action="store_true")
parser.add_argument("command", nargs=argparse.REMAINDER)
args = parser.parse_args()
command = args.command[1:]
if not args.shell:
    command += ["-append", f"stage={args.stage}"]

def check_boot(output):
    reports = re.findall(rb"hart=(\d+) root=([0-9a-f]+) ticks=(\d+)", output)
    assert len(reports) == 2 and {h for h, _, _ in reports} == {b"0", b"1"}, output
    assert len({root for _, root, _ in reports}) == 1 and int(reports[0][1],16) != 0
    assert all(int(ticks) > 0 for _, _, ticks in reports)

def check_kernel():
    data = bytearray()
    deadline = time.monotonic() + 35
    injected = False
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT) as proc:
        try:
            while time.monotonic() < deadline:
                if not select.select([proc.stdout], [], [], 0.2)[0]:
                    continue
                chunk = os.read(proc.stdout.fileno(), 8192)
                if not chunk:
                    break
                data.extend(chunk)
                if not injected and b"UART_READER_WAITING" in data:
                    proc.stdin.write(b"Q")
                    proc.stdin.flush()
                    injected = True
            if time.monotonic() >= deadline:
                raise RuntimeError(f"kernel timeout: {data[-1500:]!r}")
            assert proc.wait(timeout=2) == 0, data
        finally:
            if proc.poll() is None:
                proc.kill()
    output = bytes(data).replace(b"\r", b"")
    assert injected == (args.stage >= 4) and b"PANIC" not in output, output
    print(output.decode(errors="replace").split("Boot HART MEDELEG")[-1])
    check_boot(output)
    stages = re.findall(rb"^PASS stage (\d+)$", output, re.MULTILINE)
    assert list(map(int, stages)) == list(range(1, args.stage + 1)), output
    if args.stage >= 4:
        assert output.count(b"[one-uart-write]") == 1

def check_shell():
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT) as proc:
        def prompt(marker=b"suba> ", contains=b""):
            data = bytearray()
            end = time.monotonic() + 10
            while not (data.endswith(marker) and contains in data):
                if time.monotonic() >= end or proc.poll() is not None:
                    raise RuntimeError(f"missing prompt: {data[-800:]!r}")
                if select.select([proc.stdout], [], [], 0.2)[0]:
                    data.extend(os.read(proc.stdout.fileno(), 8192))
            return bytes(data).replace(b"\r", b"")

        def issue(cmd):
            proc.stdin.write(cmd + b"\n")
            proc.stdin.flush()
            reply = prompt()
            assert reply.startswith(cmd + b"\n"), reply
            return reply[len(cmd) + 1:-len(b"suba> ")].rstrip(b"\n")

        try:
            boot = prompt()
            check_boot(boot)
            assert b"SUBA  /  SERIAL SHELL" in boot
            for title in (b"hello [NAME]", b"ps", b"calc [EXPR]", b"wc [PATH]", b"edit PATH"):
                assert title in boot and title in issue(b"apps"), title
            shell_id = issue(b"id")
            hello = issue(b"1 Mii")
            assert hello.startswith(b"Hello, Mii!\nMy task: /tasks/1/"), hello
            assert hello.split(b"My task: ")[1] != shell_id
            assert b"My task: /tasks/0/" in issue(b"on 0 hello")
            rows = issue(b"ps").splitlines()
            assert rows[0].split() == [b"HART", b"TASK", b"STATE"]
            assert {row.split()[0] for row in rows[1:]} == {b"0", b"1"}
            assert sum(b"(ps)" in row for row in rows) == 1
            assert issue(b"calc (7 + 5) * 3") == b"36"
            proc.stdin.write(b"3\r\n"); proc.stdin.flush()
            assert b"Integer calculator" in prompt(b"calc> ")
            proc.stdin.write(b"1/0\r\n"); proc.stdin.flush()
            assert b"division by zero" in prompt(b"calc> ")
            proc.stdin.write(b"6*7\r\n"); proc.stdin.flush()
            assert b"42\ncalc> " in prompt(b"calc> ")
            proc.stdin.write(b"q\r\n"); proc.stdin.flush()
            prompt()
            assert issue(b"calc 13+8") == b"21"
            proc.stdin.write(b"\x1b[A\n"); proc.stdin.flush()
            prompt(contains=b"21\r\nsuba> ")
            assert b"Unknown command" in issue(b"no-such-program")

            assert b"cat PATH" in issue(b"help")
            if args.stage >= 4:
                frame = b"\x1b[?25h"
                def keys(data):
                    proc.stdin.write(data)
                    proc.stdin.flush()

                keys(b"edit /work/draft\r\n")
                assert b"New file" in prompt(frame)
                keys("ab\r\n中文".encode() + b"\x13")
                prompt(frame, contains=b"Saved 9 bytes")
                keys(b"\x11")
                prompt()
                assert issue(b"cat /work/draft") == "ab\n中文".encode()

                # Reopen on the first line. Move to the second, erase back
                # across its newline, and save the single remaining byte.
                keys(b"5 /work/draft\n")
                prompt(frame)
                keys(b"\x1b[B\x1b[F" + b"\x7f" * 4 + b"\x13")
                prompt(frame, contains=b"Saved 1 bytes")
                keys(b"x\x11")
                prompt(frame, contains=b"Unsaved changes")
                keys(b"\x11")
                prompt()
                assert issue(b"cat /work/draft") == b"a"
                keys(b"edit /work/draft\n")
                prompt(frame)
                keys(b"\x1b[3~\x13")
                prompt(frame, contains=b"Saved 0 bytes")
                keys(b"\x11")
                prompt()
                assert issue(b"cat /work/draft") == b""
                assert issue(b"put /work/note hello") == b"written 5"
                assert issue(b"cat /work/note") == b"hello"
                assert issue(b"wc /work/note") == b"0 lines  1 words  5 bytes  /work/note"
                assert issue(b"4").endswith(b"bytes  /svc/memory")
                assert {b"dev",b"tasks",b"work"} <= set(issue(b"ls /").splitlines())
                assert issue(b"ls /tasks").splitlines() == [b"0",b"1"]
                assert issue(b"create /work/chapter") == b"created"
                assert issue(b"create /work/chapter") == b"error Conflict"
                assert issue(b"put /work/chapter body") == b"written 4"
                assert b"chapter" in issue(b"ls /work").splitlines()
                assert issue(b"on 0 mount /dev record; put /dev/cons child; cat /dev/cons") == b"mounted\nwritten 5\nchild"
                assert issue(b"on 0 id").startswith(b"/tasks/0/")
                trace = issue(b"cat /svc/trace/0")
                if args.no_trace:
                    assert trace == b"tracing disabled", trace
                else:
                    assert trace.startswith(b"hart=0 overwritten=") and b" deliver " in trace, trace
                assert b"rx_dropped=0" in issue(b"cat /svc/uart")
                assert b"count" in issue(b"ls /out").splitlines()
                assert b"page_size=4096 free_frames=" in issue(b"cat /svc/memory")
                assert re.fullmatch(rb"tail=\d+ applied0=\d+ applied1=\d+ pending0=\d+ pending1=\d+",issue(b"cat /svc/replicas"))
                path = issue(b"id")
                assert re.fullmatch(rb"/tasks/1/[0-9]+-[0-9]+/status", path)
                assert b"hart=1" in issue(b"cat " + path)
            if args.stage == 6:
                assert issue(b"open /work/version") == b"opened"
                assert issue(b"migrate") == b"error Busy"
                assert issue(b"replace") == b"generation 2"
                assert issue(b"read") == b"v1"
                assert issue(b"close") == b"closed"
                assert issue(b"cat /work/version") == b"v2"
                assert issue(b"put /work/note kept") == b"written 4"
                assert issue(b"migrate") == b"generation 3"
                assert issue(b"cat /work/note") == b"kept"
                assert issue(b"create /work/chapter") == b"created"  # replace installed an empty tree
                assert issue(b"put /work/chapter second") == b"written 6"
                assert issue(b"migrate") == b"generation 4"
                assert issue(b"cat /work/chapter") == b"second"
                assert issue(b"cat /work/note") == b"kept"
                assert issue(b"cat /work/cons") == b""
                assert issue(b"put /out/cons abc") == b"abcwritten 3"
                assert issue(b"put /svc/counter/ctl migrate record") == b"written 14"
                assert issue(b"put /out/cons xyz") == b"written 3"
                assert issue(b"cat /out/count") == b"6"
                assert issue(b"on 0 mount /r record; cat /r/cons") == b"mounted\nxyzld"
                assert issue(b"put /svc/record/ctl migrate v2") == b"error Busy"
                assert issue(b"put /svc/record/ctl withdraw") == b"written 8"
                assert issue(b"on 0 mount /r record; cat /r/note") == b"error NotFound"
                assert b"draining=" in issue(b"cat /svc/record/status")
                assert issue(b"put /svc/record/ctl publish v2") == b"written 10"
                assert issue(b"put /svc/counter/ctl migrate console") == b"written 15"
                assert issue(b"cat /out/count") == b"6"
            proc.stdin.write(b"halt\n")
            proc.stdin.flush()
            tail, _ = proc.communicate(timeout=5)
            assert proc.returncode == 0 and b"PANIC" not in tail, tail
        finally:
            if proc.poll() is None:
                proc.kill()
    print("Production shell UART checks passed")

check_shell() if args.shell else check_kernel()
