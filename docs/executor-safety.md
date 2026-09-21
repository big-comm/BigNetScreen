# Safe execution in a constrained review environment

Before work, read `free -h`, `df -h /mnt/data`, `nproc`, and cgroup `memory.max`, `memory.current`, `memory.events`. The review limit is 4 GiB with no swap, regardless of the host's larger total. Check active command PID/PGID before retrying.

Use a single canonical checkout, isolated Rust 1.98.1, one target directory and one heavy-command lock. For global gates use jobs=1, incremental=0 and dev/test debug=0. Preserve the full source vendor shipped with the product; external dependency/toolchain caches do not belong in checkpoints.

Long commands need an internal deadline, bounded log, recorded command/cwd/compiler, PID/PGID, exit code and cleanup of the process group. Poll with short reads when streaming sessions are unavailable. Never start a second Cargo gate merely because the first transport call ended. Finish or terminate every owned job before returning a final handoff.

If `/bin/true`, `/bin/echo ok` or `python3 -S -c 'print("ok")'` fails, stop heavy work and classify ENV until evidence shows otherwise. Stdlib Python helpers can use `-S` to avoid unrelated site startup hooks; do not use it for applications needing installed Python packages. Do not modify the system Python environment to fix a review harness.

Page cache contributes to the cgroup budget. Reuse archives/builds and monitor sampled peak plus memory events. Avoid blanket system cache drops. Release only unneeded file cache if supported, or stop at a conservative guard before OOM. Do not lower the application correctness bar to make a sandbox green.

Exit 127/missing tools, missing GStreamer plugins, test assertions and compiler errors are different failure categories. Missing output files are not evidence that an unobserved build passed. Retain failed logs and fix the cause before rerunning under a new gate name.

Checkpoint with Git/bundle and hashes, restore into another directory, verify HEAD/tree, run git fsck and a targeted test. `/mnt/data` may be recreated; export verified source artifacts, not just a narrative or a script claiming to generate them later.
