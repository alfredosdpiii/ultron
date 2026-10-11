"""Protocol tap for the kernel fixtures: run runtime.py behind a relay that logs every protocol line.

Invoked by a supervisor as `python tap.py --protocol-fds` (the supervisor's runtimePath). The real runtime path and
the log file come from ULTRON_TAP_RUNTIME and ULTRON_TAP_LOG. The runtime gets fresh pipes as its fd 3 and fd 4 and
inherits fd 0, 1 and 2, so raw output reaches the supervisor unchanged. Each log line is JSON: {"dir": "k2h" or
"h2k", "line": <protocol line without its newline>}. The tap exits as the runtime did (same code or signal).
"""
import json
import os
import signal
import subprocess
import sys
import threading

runtime = os.environ["ULTRON_TAP_RUNTIME"]
log = open(os.environ["ULTRON_TAP_LOG"], "a", encoding="utf-8")
lock = threading.Lock()

host_out = os.dup(3)  # frames to the supervisor
host_in = os.dup(4)  # messages from the supervisor
kernel_out_r, kernel_out_w = os.pipe()
kernel_in_r, kernel_in_w = os.pipe()
os.dup2(kernel_out_w, 3)
os.dup2(kernel_in_r, 4)
for fd in (kernel_out_w, kernel_in_r):
    os.close(fd)
for fd in (host_out, host_in, kernel_out_r, kernel_in_w):
    os.set_inheritable(fd, False)
child = subprocess.Popen([sys.executable, runtime, *sys.argv[1:]], pass_fds=(3, 4))
os.close(3)
os.close(4)


def relay(source: int, target: int, direction: str) -> None:
    reader = os.fdopen(source, "rb")
    while True:
        line = reader.readline()
        if not line:
            break
        with lock:
            log.write(json.dumps({"dir": direction, "line": line.rstrip(b"\n").decode("utf-8")}, ensure_ascii=False) + "\n")
            log.flush()
        view = memoryview(line)
        try:
            while view:
                view = view[os.write(target, view):]
        except OSError:
            break
    os.close(target)


threading.Thread(target=relay, args=(host_in, kernel_in_w, "h2k"), daemon=True).start()
reader = threading.Thread(target=relay, args=(kernel_out_r, host_out, "k2h"), daemon=True)
reader.start()
code = child.wait()
reader.join()
if code < 0:
    if -code not in (signal.SIGKILL, signal.SIGSTOP):
        signal.signal(-code, signal.SIG_DFL)
    os.kill(os.getpid(), -code)
os._exit(code)
