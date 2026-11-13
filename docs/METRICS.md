# Prometheus textfile output

The collector atomically publishes `<DATA_DIR>/metrics.prom` every 15s by
default. Set `SLURM_GPU_TUI_METRICS_FILE` and `SLURM_GPU_TUI_METRICS_SEC` to
change the path/cadence. Point an existing node_exporter textfile collector at
this directory; its service must see the same directory (a private `/tmp` mount
would hide the default path). Prometheus/node_exporter installation is managed
by the site.

Important series include:

| Series | Meaning |
|---|---|
| `sgpu_collector_last_success_timestamp_seconds` | Snapshot publication heartbeat |
| `sgpu_build_info` | Native build identity |
| `sgpu_nodes_total`, `sgpu_nodes_up`, `sgpu_nodes_stale` | Inventory/publication status |
| `sgpu_node_data_age_seconds`, `sgpu_node_scheduler_age_seconds` | Observation ages; -1 means unknown |
| `sgpu_gpus_total`, `sgpu_gpus_allocated`, `sgpu_gpus_free` | GPU inventory/allocation/schedulable free capacity |
| `sgpu_jobs_running`, `sgpu_jobs_pending` | Full snapshot counts |
| `sgpu_gpu_*`, `sgpu_node_*` | GPU utilization/health, RAM, CPU, PSI, power and identity |
| `sgpu_job_*` | Validated job resources, requested RAM/fair-share and observation coverage |
| `sgpu_pending_job_info`, `sgpu_pending_wait_seconds` | Bounded per-job pending details |
| `sgpu_collector_*`, `sgpu_command_*` | Cycle/phase duration, bounded queue depths and fixed-label local command counters |
| `sgpu_master_*` | Local master's CPU, memory, disk, network, filesystem and hardware observations |

Unsupported, unverified or stale optional measurements are omitted; they are
not converted to zero. GPU RAM/node RAM use MiB, power W, clocks MHz. The default
per-job pending cap is 100 (`SLURM_GPU_TUI_METRICS_PENDING_MAX`); the aggregate
pending count remains uncapped. Source log paths and credentials are excluded.

An existing Prometheus can load these alert rules. External heartbeat alerts
complement collector Slack notifications because a stopped collector cannot
notify about itself. Validate the site's rule file with `promtool check rules`.

```yaml
groups:
  - name: sgpu
    rules:
      - alert: SgpuCollectorStale
        expr: time() - sgpu_collector_last_success_timestamp_seconds > 90
        for: 2m
        labels:
          severity: critical
        annotations:
          summary: sgpu collector data is stale
          description: >-
            The sgpu collector has not published a successful snapshot for
            more than 90 seconds. Check sgpu-collector and its journal.

      - alert: SgpuCollectorMetricMissing
        expr: absent(sgpu_collector_last_success_timestamp_seconds)
        for: 5m
        labels:
          severity: critical
        annotations:
          summary: sgpu collector metric is missing
          description: >-
            Prometheus cannot see the sgpu collector heartbeat. Check the
            node_exporter scrape target and its textfile collector path.

      - alert: JobMemOverFairShare
        expr: sgpu_job_mem_fair_ratio > 1.0
        for: 30m
        labels:
          severity: warning
        annotations:
          summary: "job {{ $labels.jobid }} ({{ $labels.user }}) holds RAM beyond its GPU share"
          description: >-
            Job {{ $labels.jobid }} by {{ $labels.user }} on
            {{ $labels.node }} requests {{ $value }}x the RAM its
            {{ $labels.gpus }} GPU(s) entitle it to (node RAM divided by
            node GPU count). Other GPU users on that node may be starved.
```
