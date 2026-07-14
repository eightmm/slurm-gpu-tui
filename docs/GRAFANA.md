# Grafana Dashboard

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

## 5. Import dashboard

In Grafana:

1. Dashboards -> New -> Import.
2. Upload `grafana/sgpu-dashboard.json`.
3. Select your Prometheus data source.

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
- `sgpu_gpu_info{node,gpu,name}`
- `sgpu_gpu_util{node,gpu}`
- `sgpu_gpu_mem_used_mib{node,gpu}`
- `sgpu_gpu_mem_total_mib{node,gpu}`
- `sgpu_gpu_mem_used_percent{node,gpu}`
- `sgpu_gpu_temp_celsius{node,gpu}`
- `sgpu_gpu_power_watts{node,gpu}`
- `sgpu_gpu_allocated{node,gpu,user}`
- `sgpu_gpu_idle_seconds{node,gpu}`
- `sgpu_gpu_parked_seconds{node,gpu}`

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
