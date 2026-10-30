# Input pipeline bench B

This external Linux fixture runs **two real cores and their dry agents**, using the same harness against each binary. It never injects native desktop input. `observe.c` observes only synthetic dry-agent IPC; **never preload it into a real/native session**. The agent still owns no network sockets.

## Prerequisites and isolation

- Windows Docker Desktop in Linux-container mode; image **`lws-ci-linux-deps:rust1.99`**. Keep the image tag and record its image ID and `rustc --version` with the results.
- Python 3.10+, GCC, Xvfb/xauth and Linux `iproute2` with `tc netem ... seed` support inside the container. The named image already supplies the Rust/build/Xvfb dependencies; install `iproute2` in this dedicated container if absent.
- A separate v0.3.3 worktree and a current working-tree snapshot, outside the repository, plus separate Cargo target volumes. Do not point `robocopy /MIR` at a directory containing unrelated work.
- **Only the dedicated bench container gets `NET_ADMIN`; never use `--privileged` or host networking.** The runner refuses to run outside Docker and changes only that container's `lo` qdisc. Do not reuse a container running another workload.

The recorded paired builds used the image-default **rustc 1.99.0**, dev/unoptimized, debug=0 and incremental=0 on both sides. Build from `/fixture` as below: changing into a mounted source tree may select a different rustup toolchain through its toolchain file. Do not mix compiler cohorts silently. Version strings still being 0.3.3 before release preparation are expected.

## Exact Windows/PowerShell setup

Run from the repository root. Pick fresh result paths; the runner refuses to overwrite JSONL. The v0.3.3 CLI binary is also named `bifrost`, despite its older internal crate names.

```powershell
$Repo = (Get-Location).Path
$Bench = "bf-input-bench-$PID"
$BeforeTarget = "bf-input-before-$PID"
$AfterTarget = "bf-input-after-$PID"
New-Item -ItemType Directory -Force C:\bf-bench-results | Out-Null
git worktree add C:/bf-before v0.3.3
robocopy $Repo C:\bf-after /MIR /XD target /R:1 /W:1
if ($LASTEXITCODE -ge 8) { throw "snapshot failed" }
# robocopy exit codes 0..7 are not copy failures; .git is included.
docker run -d --name $Bench --cap-add NET_ADMIN --tmpfs /tmp:exec `
  -e CARGO_BUILD_JOBS=4 -e CARGO_INCREMENTAL=0 `
  -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 `
  -v "C:\bf-before:/before:ro" -v "C:\bf-after:/after:ro" `
  -v "${Repo}/apps/bifrost/tests/bench_b:/fixture:ro" `
  -v "C:\bf-bench-results:/results" `
  -v "${BeforeTarget}:/target-before" -v "${AfterTarget}:/target-after" `
  -w /fixture lws-ci-linux-deps:rust1.99 sleep infinity

docker image inspect lws-ci-linux-deps:rust1.99 --format '{{.Id}}'
docker exec $Bench bash -c 'export PATH=/usr/local/cargo/bin:$PATH; rustc --version; tc -V'
# If tc is absent:
docker exec $Bench bash -c 'apt-get update && apt-get install -y --no-install-recommends iproute2'
docker exec $Bench gcc -shared -fPIC -O2 -o /tmp/libbf_bench_observer.so /fixture/observe.c -pthread
docker exec -w /fixture $Bench python3 -B -m unittest test_observe test_report test_seed_report
docker exec -w /fixture $Bench bash -c 'export PATH=/usr/local/cargo/bin:$PATH && CARGO_TARGET_DIR=/target-before cargo build --locked --manifest-path /before/Cargo.toml -p bifrost -j 4 && CARGO_TARGET_DIR=/target-after cargo build --locked --manifest-path /after/Cargo.toml -p bifrost -j 4'
```

For Git Bash/MSYS, prefix Docker invocations with `MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'` so Linux mount and executable paths are not rewritten. The examples above use PowerShell and need no such prefix.

## Affected-profile comparison: 12 runs

Loss 1/5%, added delay/jitter 0/0, seeds 17/23/41, **30 seconds measured per run**, both binaries:

```powershell
docker exec -w /fixture $Bench xvfb-run -a -s '-screen 0 1280x1024x24 -noreset' python3 -B /fixture/run.py --side before --binary /target-before/debug/bifrost --observer /tmp/libbf_bench_observer.so --seconds 30 --seeds 17,23,41 --losses 1,5 --delays 0:0 --cases matrix --root /tmp/bb --output /results/before.jsonl --stop-on-error
docker exec -w /fixture $Bench xvfb-run -a -s '-screen 0 1280x1024x24 -noreset' python3 -B /fixture/run.py --side after --binary /target-after/debug/bifrost --observer /tmp/libbf_bench_observer.so --seconds 30 --seeds 17,23,41 --losses 1,5 --delays 0:0 --cases matrix --root /tmp/bb --output /results/after.jsonl --stop-on-error
docker exec -w /fixture $Bench python3 -B /fixture/seed_report.py /results/before.jsonl /results/after.jsonl --json /results/per-seed.json --csv /results/per-seed.csv
```

For the full matrix, run each side with `--losses 0,1,5 --delays 0:0,10:5,30:10 --seeds 17,23,41 --seconds 30 --cases matrix,stalled_reader,blackout_500ms`, choosing **new output JSONL paths**. This is 27 matrix profiles per side plus two fault cases.

`stalled_reader` pauses the fixture's TCP proxy for 2 s; `blackout_500ms` applies 100% loss for 500 ms. Both report control return and fresh-lease recovery, without changing the product's lease deadlines.

`report.py before.jsonl after.jsonl --json summary.json --csv summary.csv` reports per-profile seed medians and fault results; its overall sections contain counts, not cross-loss latency quantiles. Use `seed_report.py` for the affected D0 comparison and abort-censored per-seed latency.

Always clean up your own container and baseline worktree, including after a failed run. Keep result files and target volumes if needed for analysis:

```powershell
docker stop $Bench
docker rm $Bench
git worktree remove C:/bf-before
```

## Definitions and limits

- Synthetic absolute motion is published at 100 Hz; every 25th update generates a click (4 clicks/s). The fixture never waits for an input ACK before publishing the next update. The final point is verified at the source agent, publication stops, and a 2 s delivery drain does not create a rescue sample.
- The common before/after metric is **same-kernel CLOCK_MONOTONIC CapturedBatch successful publish → dry receiver InjectResult successful publish**. It includes software capture publication, TCP delivery and receiver work; it is not hardware HID latency or a cross-machine synchronized-clock measurement.
- `pub_to_apply` is unchanged across comparisons. Repeated pointer identities that cannot be matched unambiguously are excluded from timing identity; their count is reported. `pub_to_apply_discrete` is an additional exact-FIFO diagnostic, not a replacement metric and not permission to bypass native input tracking.
- All input, including motion, retains exact native lease+sequence completion tracking. Current native admission is 64 offered + 64 pending locally; input is not gated by reverse-network InjectAck. No timeout/lease grace is adjusted for any profile.
- `seed_report.py` retains observed diagnostics but excludes aborted/unavailable/oracle-invalid leases from **completed** latency; missing quantiles remain null, not zero. Do not pool losses or replace an unsuccessful seed with a later successful repetition. A repeated run is a separate diagnostic cohort.
- Stalls are receiver application gaps over 50 ms; `stall_excess_ms` sums the gap portions above 50 ms. Check wrong clicks, missing/disordered discrete input, cross-lease leaks, final position and source lease survival separately. Fault control-return acceptance is TTL + 100 ms (1100 ms at the default TTL); a normal matrix run has no fault control-return measurement.
- JSONL includes namespace-local TCP retransmission/timeout/segment deltas and qdisc bytes/packets/drops. Counter scope includes the measured interval and delivery drain; these are not application frame counts. After-only `input-metrics.log` provides bounded capture→enqueue→write, TLS write, inbound dispatch and InjectAck timing; v0.3.3 cannot supply equivalent capture timestamps. Never substitute Ping RTT for input age.
- The same netem seed is **not identical packet-by-packet replay** across versions: handshake direction, OS scheduling, frame sizes and packet interleaving can change the loss mapping. This loopback/dry bench is not a Wi-Fi measurement and proves no universal p99 improvement or non-regression.

## Recorded final native-local acceptance (2026-10-30)

All profiles below used D/J=0/0 and 30 s. Quantiles are the unchanged common metric in milliseconds; no aborted lease contributes a completed latency value. Wrong click/order/cross-lease counts and injection errors were 0 in every final after run.

| Loss | Seed | Before p95/p99 | After p95/p99 | Stalls before/after | After final point |
|---|---|---|---|---|---|
|1%|17|1.13 /1.48|1.44 /6.10|0 /0|delivered|
|1%|23|1.12 /2.30|1.20 /6.53|0 /0|delivered|
|1%|41|1.08 /1.53|1.47 /5.46|0 /0|delivered|
|5%|17|21.82 /175.19|115.43 /332.44|8 /10|delivered|
|5%|23|83.58 /195.58|170.41 /490.53|11 /14|delivered|
|5%|41|excluded: LeaseExpired|88.35 /348.61|5 /8|delivered|

This is 6/6 reliable final deliveries, **not a latency improvement claim**. Prior aborting seeds remain in the retained evidence; the old 65th-offer eviction was reproduced on both v0.3.3 and the earlier working tree.

In a separate payload-free current-version loss 5%/D0/seed 23 diagnostic, unchanged-code after p99 was 194.42 ms rather than 490.53 ms; TCP send→read p99 was 192.68 ms, receiver read→agent offer 3.30 ms and offer→result 0.37 ms. The corresponding before values were 191.85 ms overall, 189.04 ms TCP, 3.55 ms receiver and 0.40 ms adapter.

These repeated-cohort figures identify TCP delivery tails and measured variation; they do not erase the slower final-matrix seed. Temporary stage probes were removed, not shipped as product telemetry.
