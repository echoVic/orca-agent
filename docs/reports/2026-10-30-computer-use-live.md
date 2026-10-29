# Computer-use live acceptance — v0.5.14

Date: 2026-10-30 · HEAD: `8fe47f7d` · binary sha256 `430fffce…`
Harness: `scripts/eval/computer_use/run.py`, live mode, tasks `S1`, `M2`, `M7`
Precondition: Accessibility and screen recording granted (`ax_trusted: true`, `screen_capture: true`);
the earlier rounds could only run the mock dry-run.

## Result: 3/3 live tasks pass

| Task | Verified | Terminal | Seconds | Turns | Tools | Computer actions | Errors |
|---|---|---|---:|---:|---:|---|---:|
| S1 | **pass** | completed | 25.0 | 15 | 14 | 14 (click 10, launch 1, list 2, observe 1) | 1 |
| M2 | **pass** | completed | 17.2 | 9 | 8 | 8 (click 2, key 2, launch 1, list 1, type 2) | 0 |
| M7 | **pass** | completed | 41.0 | 16 | 16 | 13 (click 1, key 8, launch 1, list 1, observe 1, type 1) | 1 |

Aggregates: single 1/1, multi 2/2, strict GUI success 2/2, 35 computer calls total,
~83 s wall, ~$0.016 estimated. Cleanup and restoration checks are in the run's
`environment.json` / evidence directory.

## What this adds to the program

The computer-use axis now has a live measurement instead of only the protocol-level mock smoke:
the agent drives real windows (including multi-app tasks) and the harness verifies outcomes from the
GUI state, with approval interactions recorded.

Reproduce (requires the permissions above):

```bash
python3 -I scripts/eval/computer_use/run.py --tasks S1,M2,M7 --out /tmp/orca-cu-live
```
