# Grafana Dashboard

## Quick install (Ubuntu/Debian, collector host)

```bash
sudo grafana/install.sh
```

One idempotent script sets up the full stack: node_exporter (textfile
collector, `127.0.0.1:9100`), Prometheus (scrape + sgpu alert rules,
`127.0.0.1:9090`), and Grafana (provisioned datasource + this repo's
dashboard, `0.0.0.0:3000` — login required, sign-up disabled; create Viewer
accounts for read-only users). Every unit gets `Restart=always` so site
cron kill sweeps cannot leave the stack dead. The sections below describe
the same setup step by step for other distros or custom layouts.

`sgpu-collector` writes Prometheus textfile metrics to:

```bash
/tmp/slurm-gpu-tui/metrics.prom
```

Use node_exporter's textfile collector to expose that file, then scrape
node_exporter from Prometheus and import `grafana/sgpu-dashboard.json`.

> **PrivateTmp trap (read this first).** Many distro node_exporter systemd
> units ship `PrivateTmp=yes`, giving node_exporter a *private* `/tmp` — it
> will never see the file above and the dashboard stays empty with no error.
> Either add a `PrivateTmp=no` drop-in to node_exporter, or write the metrics
> somewhere shared:
>
> ```bash
> export SLURM_GPU_TUI_METRICS_FILE=/var/lib/node_exporter/textfile/sgpu.prom
> ```
>
> set on the collector service, and point node_exporter's
> `--collector.textfile.directory` at that directory. `sgpu doctor` honors the
> same override and reports the active path.

## 1. Check sgpu metrics

```bash
sgpu doctor
ls -l /tmp/slurm-gpu-tui/metrics.prom
sed -n '1,80p' /tmp/slurm-gpu-tui/metrics.prom
```

The file is rewritten every collector cycle.

## 2. Expose with node_exporter

If node_exporter is already installed, add this flag:

```bash
--collector.textfile.directory=/tmp/slurm-gpu-tui
```

Example systemd drop-in:

```ini
[Service]
ExecStart=
ExecStart=/usr/local/bin/node_exporter --collector.textfile.directory=/tmp/slurm-gpu-tui
```

Then restart:

```bash
sudo systemctl daemon-reload
sudo systemctl restart node_exporter
curl -s http://localhost:9100/metrics | grep '^sgpu_'
```

## 3. Scrape from Prometheus

Minimal scrape job:

```yaml
scrape_configs:
  - job_name: sgpu
    static_configs:
      - targets:
          - login-node:9100
```

Reload Prometheus and confirm:

```promql
sgpu_gpus_total
sgpu_gpu_util
```

## 4. Alert when the collector dies

The collector cannot send its own Slack message after it has stopped. Load the
included Prometheus rules so Alertmanager provides the external dead-man check:

```yaml
rule_files:
  - /etc/prometheus/rules/sgpu-alerts.yml
```

```bash
sudo install -m 644 prometheus/sgpu-alerts.yml /etc/prometheus/rules/sgpu-alerts.yml
promtool check rules /etc/prometheus/rules/sgpu-alerts.yml
sudo systemctl reload prometheus
```

The rules cover both cases:

- `SgpuCollectorStale`: the textfile remains present but its timestamp stops.
- `SgpuCollectorMetricMissing`: the metric or node_exporter scrape disappears.

Route `severity="critical"` to Slack in Alertmanager. This is separate from
the collector's built-in Slack alerts, which cover node and GPU conditions
while the collector itself is alive.

`grafana/install.sh` automates this: it installs `prometheus-alertmanager`
(bound to `127.0.0.1:9093`), points Prometheus at it, and builds a Slack
route from the collector's own credentials in `/root/.sgpu/slack.json`
(bot token + channel, posted via `chat.postMessage`). If that file is
missing the alertmanager starts with a null route — fill in
`/etc/prometheus/alertmanager.yml` by hand. The installer also sets
`--storage.tsdb.retention.time=180d` so power/usage history keeps for six
months (~1 GB at this cluster's series count).

## 5. Import dashboards

Two dashboards ship in `grafana/` (the installer provisions both):

- `sgpu-dashboard.json` — cluster overview: summary gauges/stats, master
  block (node_exporter), per-node repeated rows, collapsed cluster trends.
- `sgpu-node-detail.json` — single-node drill-down: per-GPU repeated rows
  (card model, util/mem trend + gauges, temp, power, user·job), aggregate
  trends, power split, idle/parked, allocation table. Linked both ways
  with the overview.

Manual import: Dashboards -> New -> Import -> upload the JSON -> select
your Prometheus data source.

`prometheus/sgpu-master-bridge.yml` (recording rules) bridges the master's
node_exporter metrics into `sgpu_node_*{node="master"}` so the Node
dropdown includes the login node. Install it with the alert rules
(section 4) — the installer already copies every `prometheus/*.yml`.

## Metrics

Cluster summary:

- `sgpu_jobs_running`
- `sgpu_jobs_pending`
- `sgpu_nodes_total`
- `sgpu_nodes_up`
- `sgpu_nodes_stale`
- `sgpu_gpus_total`
- `sgpu_gpus_allocated`
- `sgpu_gpus_free`
- `sgpu_gpus_idle`
- `sgpu_gpus_parked`
- `sgpu_gpus_rogue`
- `sgpu_collector_last_success_timestamp_seconds` (Unix time of the snapshot;
  `time() - <this>` = data age. The dashboard's **Data Age** stat goes
  orange/red when the collector freezes or dies.)
- `sgpu_build_info{version,build}` (exact collector build; compare with `sgpu --version`)

Per node/GPU:

- `sgpu_node_up{node}`
- `sgpu_node_stale{node}`
- `sgpu_node_info{node,partition,source}`
- `sgpu_node_cpus_total{node}` / `sgpu_node_cpus_alloc{node}` (Slurm view)
- `sgpu_node_cpu_load{node}` (load average)
- `sgpu_node_mem_total_mib{node}` / `sgpu_node_mem_used_mib{node}` /
  `sgpu_node_mem_alloc_mib{node}` / `sgpu_node_mem_avail_mib{node}`
- `sgpu_node_cpu_power_watts{node}` / `sgpu_node_ram_power_watts{node}` —
  RAPL package/DRAM power measured by the node agent (needs root; RAM only
  on Intel). CPU+DRAM only — no fans/board/PSU losses, so this is not
  wall-plug power. GPU power is separate (`sgpu_gpu_power_watts`).
- `sgpu_node_sys_power_watts{node}` — whole-node wall power from the BMC
  (`ipmitool dcmi power reading`; needs root + `/dev/ipmi0`, i.e. the
  `ipmi_devintf` module). Read every ~10s and cached between agent cycles;
  absent when the node has no BMC or ipmitool.
- `sgpu_gpu_info{node,gpu,name}`
- `sgpu_gpu_util{node,gpu}`
- `sgpu_gpu_mem_used_mib{node,gpu}`
- `sgpu_gpu_mem_total_mib{node,gpu}`
- `sgpu_gpu_mem_used_percent{node,gpu}`
- `sgpu_gpu_temp_celsius{node,gpu}`
- `sgpu_gpu_power_watts{node,gpu}`
- `sgpu_gpu_allocated{node,gpu,user}`
- `sgpu_gpu_job_info{node,gpu,user,jobid,jobname}` (only while allocated)
- `sgpu_gpu_idle_seconds{node,gpu}`
- `sgpu_gpu_parked_seconds{node,gpu}`
- `sgpu_gpu_sm_clock_mhz{node,gpu}` / `sgpu_gpu_mem_clock_mhz{node,gpu}`
- `sgpu_pending_job_info{jobid,user,partition,jobname,reason,gpus}` (one
  series per queued job; disappears when the job starts)

Per running GPU job (RAM fair share):

- `sgpu_job_mem_mib{jobid,user,node,gpus}` — RAM the job *requested*
  (`squeue %m`; allocation, not usage). Absent when the job requests no
  memory or holds no GPUs.
- `sgpu_job_mem_fair_ratio{jobid,user,node,gpus}` — requested RAM over the
  job's GPU fair share (node RAM × job GPUs ÷ node GPUs). `> 1` = the job
  reserves more memory than its GPU count entitles it to.
  `prometheus/sgpu-alerts.yml` ships a 30-minute warning rule on it.

Master host (the machine the collector runs on — lets a remote Grafana
monitor this cluster without node_exporter here):

- `sgpu_master_*` — CPU idle counters, memory, load1, boot time, local
  filesystems, network/disk byte counters, coretemp, hwmon power. Metric
  suffixes mirror node_exporter's (`sgpu_master_cpu_seconds_total`, …), so
  consumers translate mechanically: `node_X` → `sgpu_master_X`.

Node power coverage: `sudo ./setup-node-power.sh` (repo root) probes every
SLURM node and installs/persists what BMC wall power and RAPL need;
`sgpu doctor` reports remaining gaps.

## Multi-cluster: bridge another sgpu install

One Grafana can watch several sgpu clusters without label collisions or
cross-firing alert rules — remote series are republished under a
metric-name prefix:

- `grafana/sgpu-remote-bridge.sh` — fetches the remote cluster's
  `metrics.prom` over ssh every 30 s and rewrites `sgpu_*` →
  `<prefix>sgpu_*` into the local textfile dir. The ssh target and prefix
  come from `~/.config/sgpu/bridge.env` (site file, not in git — copy
  `grafana/bridge.env.example`).
  On fetch failure the data series are dropped (panels go honest
  "No data") and only `<prefix>sgpu_bridge_up 0` remains. For remotes on
  an older sgpu without `sgpu_master_*`, it falls back to sampling the
  remote master's /proc over ssh.
- `sgpu-master-bridge.service` / `.timer` — user units
  (`systemctl --user`, host needs linger) running the bridge every 30 s.
- `grafana/gen-master-dashboards.py` — regenerates the bridged cluster's
  dashboard pair from the local sources (prefix rewrite, uids, titles,
  cross-links). Rerun after editing the local dashboards.
- `grafana/gen-overview-dashboard.py` — the "All Clusters Overview"
  dashboard: combined totals (wall power, GPUs, jobs, waste), per-cluster
  comparison, trends. No per-node panels.
- `prometheus/sgpu-master-cluster-alerts.yml` — bridge down / remote
  collector stale / remote node down / remote RAM-over-share rules for the
  prefixed series (stock rules only match local `sgpu_*`).

## Notes

- Grafana is read-only here; Slack alerts still come from `sgpu-collector`.
- Do not expose node_exporter publicly. Put it behind your existing monitoring
  network or firewall.
- If node_exporter runs in a container, mount the metrics dir read-only into
  the container at the same path or adjust the flag.
- `sgpu_gpu_allocated{...,user=...}`: when a GPU is freed the old `user="alice"`
  series stops and a `user=""` series starts, so Prometheus keeps one stale
  series per (gpu,user) pair ever seen. Harmless at SLURM scale (series ≈ GPUs),
  but don't `group by (user)` without an `offset`/staleness guard.
- If the metrics file lands on **NFS**, `rename()` is not atomic across the
  wire — prefer a local path for `SLURM_GPU_TUI_METRICS_FILE`, same as the live
  `data.json` which stays in local `/tmp` by design.
