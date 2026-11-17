# Performance checks

Build the release binary before comparing timings:

```bash
cargo build --locked --release --bin sgpu -j 2
target/release/sgpu bench --nodes 128 --repeat 21
target/release/sgpu bench --suite --nodes 128 --jobs 10000 --repeat 21
target/release/sgpu bench --replay snapshot.json --repeat 21
```

The default five cases cover identical data, one utilization change, missing
metrics, a stale node and collapsed nodes. `--suite` also covers Jobs priority,
wait and remaining-time sorting, Attention, Users, search, free-GPU sorting and
Host. `--jobs` sets the pending-job count in Jobs and Users cases (default 1000,
maximum 100000); running jobs remain one per synthetic node.

Each case reports median milliseconds for JSON parsing, `App::apply`, drawing,
and the combined apply/draw operation. Fixture generation, input copying and
serialization occur outside those timings. The combined value excludes parsing;
medians of separate phases need not sum to the median combined value. Older
benchmarks included an extra snapshot copy in the combined timing.

The renderer uses a 160×50 in-memory terminal. Synthetic nodes have eight GPUs;
cases expand all nodes except `collapsed`, overriding the normal automatic
collapse for large clusters. Timings include widget preparation and buffer
updates, but exclude terminal output, Slurm RPCs and NFS access. Replay accepts
one snapshot, a nonempty array, or an object with a `snapshots` array.

Compare the same release build, inputs and machine under similar load. The
small CI smoke benchmark checks execution; it has no timing threshold. Run the
larger suite for performance changes and preserve numeric identity, freshness,
unknown values, sorting ties, CPU distribution and mouse/keyboard behavior.

## Example comparison

Local medians from 21 repetitions alternating the actual 2.4.2 and 2.4.3 release
libraries within one process:

| Operation | 2.4.2 | 2.4.3 |
|---|---:|---:|
| Collector merge, 512 nodes with missing agent payloads | 90.62 ms | 3.45 ms |
| Wait sort, 10000 pending jobs | 154.36 ms | 7.94 ms |
| Jobs draw, 128 nodes and 10000 pending jobs | 73.33 ms | 7.96 ms |
| GPU draw, 128 fully expanded nodes | 6.27 ms | 1.10 ms |
| Snapshot decode, 128 nodes and 10000 pending jobs | 115.12 ms | 87.07 ms |

The draw comparisons produced identical terminal buffers. Collector inputs used
isolated missing-file paths; these figures do not measure live NFS delivery or
whole-cluster collection. Thread CPU timings supported the same improvements;
absolute times depend on hardware, allocator state and competing work.
