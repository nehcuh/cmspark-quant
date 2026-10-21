#!/usr/bin/env python3
"""Generate deploy/seccomp/cmsq-runner.json from Docker's default profile.

Starts from moby's default (deploy/seccomp/moby-default-v26.1.5.json, verbatim from
github.com/moby/moby/blob/v26.1.5/profiles/seccomp/default.json) and removes the
syscalls that read the wall clock directly, so a strategy that bypasses libfaketime
with a raw syscall gets EPERM instead of the real date:
  time, gettimeofday, adjtimex, clock_adjtime(64)      -> denied
  clock_gettime(64)                                    -> only CPU-time and monotonic
                                                          clocks (1,2,3,4,6) allowed
Normal reads go through libc -> libfaketime (frozen 2000-01-01) or the vDSO and never
reach these syscalls. Residual: a strategy that calls the vDSO directly, or stats a
file it just wrote via a raw syscall, still sees the real time (see docs/STATUS.md).
"""
import json, pathlib, sys
root = pathlib.Path(__file__).resolve().parent.parent
src = json.loads((root / "deploy/seccomp/moby-default-v26.1.5.json").read_text())
CLOCK_READS = {"time", "gettimeofday", "adjtimex", "clock_adjtime", "clock_adjtime64", "clock_gettime", "clock_gettime64"}
for s in src["syscalls"]:
    if s.get("action") == "SCMP_ACT_ALLOW" and not s.get("args") and not s.get("includes") and not s.get("excludes"):
        s["names"] = [n for n in s["names"] if n not in CLOCK_READS]
ALLOWED_CLOCKS = [1, 2, 3, 4, 6]  # MONOTONIC, PROCESS_CPUTIME, THREAD_CPUTIME, MONOTONIC_RAW, MONOTONIC_COARSE
for c in ALLOWED_CLOCKS:
    src["syscalls"].append({"names": ["clock_gettime", "clock_gettime64"], "action": "SCMP_ACT_ALLOW",
                            "args": [{"index": 0, "value": c, "op": "SCMP_CMP_EQ"}],
                            "comment": "cmsq: non-wall clocks only"})
src["syscalls"].append({"names": sorted(CLOCK_READS), "action": "SCMP_ACT_ERRNO", "errnoRet": 1,
                        "comment": "cmsq: wall-clock reads denied (EPERM)"})
out = json.dumps(src, indent=1, sort_keys=False) + "\n"
dst = root / "deploy/seccomp/cmsq-runner.json"
if "--check" in sys.argv:
    sys.exit(0 if dst.read_text() == out else "deploy/seccomp/cmsq-runner.json is stale: run scripts/gen_seccomp.py")
dst.write_text(out)
print(dst)
