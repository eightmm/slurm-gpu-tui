#!/usr/bin/env python3
"""Generate the all-clusters overview dashboard (no per-node panels).

One screen answering "how are both clusters doing": combined totals up
top, per-cluster comparison stats, then trend lines. Local cluster is
sgpu_* (ai-master), the bridged cluster is master_sgpu_* (10.10.0.100).

Writes grafana/sgpu-overview.json in repo export form (${DS_PROMETHEUS}
placeholder); deploy via grafana/install.sh or its transformation.
"""
import json

DS = {"type": "prometheus", "uid": "${DS_PROMETHEUS}"}
CLUSTERS = [("ai-master", "sgpu_"), ("master (10.10.0.100)", "master_sgpu_")]

_id = 0


def nid() -> int:
    global _id
    _id += 1
    return _id


def targets(expr_fmt: str, legend_per_cluster: bool = True):
    """One query per cluster from a metric-prefix format string."""
    out = []
    for i, (name, pfx) in enumerate(CLUSTERS):
        out.append({
            "refId": chr(ord("A") + i), "datasource": DS,
            "expr": expr_fmt.format(p=pfx),
            "legendFormat": name if legend_per_cluster else "__auto",
        })
    return out


def stat(title, expr, x, y, w, unit="none", h=5, color="blue", desc=""):
    return {
        "id": nid(), "type": "stat", "title": title, "datasource": DS,
        "description": desc,
        "gridPos": {"x": x, "y": y, "w": w, "h": h},
        "fieldConfig": {"defaults": {
            "unit": unit, "color": {"mode": "fixed", "fixedColor": color},
            "decimals": 0,
        }, "overrides": []},
        "options": {"reduceOptions": {"calcs": ["lastNotNull"]},
                    "graphMode": "area", "colorMode": "value"},
        "targets": [{"refId": "A", "datasource": DS, "expr": expr}],
    }


def multistat(title, expr_fmt, x, y, w, unit="none", h=5):
    return {
        "id": nid(), "type": "stat", "title": title, "datasource": DS,
        "gridPos": {"x": x, "y": y, "w": w, "h": h},
        "fieldConfig": {"defaults": {"unit": unit, "decimals": 0}, "overrides": []},
        "options": {"reduceOptions": {"calcs": ["lastNotNull"]},
                    "graphMode": "none", "colorMode": "value",
                    "textMode": "value_and_name"},
        "targets": targets(expr_fmt),
    }


def trend(title, expr_fmt, x, y, w, unit="none", h=8, stack=False):
    p = {
        "id": nid(), "type": "timeseries", "title": title, "datasource": DS,
        "gridPos": {"x": x, "y": y, "w": w, "h": h},
        "fieldConfig": {"defaults": {
            "unit": unit,
            "custom": {"fillOpacity": 12, "lineWidth": 2, "showPoints": "never"},
        }, "overrides": []},
        "options": {"legend": {"displayMode": "list", "placement": "bottom"},
                    "tooltip": {"mode": "multi"}},
        "targets": targets(expr_fmt),
    }
    if stack:
        p["fieldConfig"]["defaults"]["custom"]["stacking"] = {"mode": "normal"}
    return p


both = "({p0}{{m}} + {p1}{{m}})"  # unused helper text; kept simple below


def summed(metric: str) -> str:
    return " + ".join(f"sum({pfx}{metric})" for _, pfx in CLUSTERS)


panels = [
    # ── combined totals ────────────────────────────────────────────────
    stat("Total Wall Power", summed("node_sys_power_watts"), 0, 0, 4, "watt", color="orange",
         desc="Both clusters, BMC-reporting nodes only (sgpu doctor lists gaps)."),
    stat("GPUs Total", summed("gpus_total"), 4, 0, 3),
    stat("GPUs Busy", summed("gpus_allocated"), 7, 0, 3, color="green"),
    stat("GPUs Free", summed("gpus_free"), 10, 0, 3, color="cyan"),
    stat("Running Jobs", summed("jobs_running"), 13, 0, 3, color="green"),
    stat("Pending Jobs", summed("jobs_pending"), 16, 0, 3, color="yellow"),
    stat("Wasted (idle+parked)", summed("gpus_idle") + " + " + summed("gpus_parked"),
         19, 0, 5, color="red",
         desc="Allocated GPUs doing no compute across both clusters."),
    # ── per-cluster comparison ─────────────────────────────────────────
    multistat("Wall Power by Cluster", "sum({p}node_sys_power_watts)", 0, 5, 6, "watt"),
    multistat("GPU Busy / Total", "sum({p}gpus_allocated)", 6, 5, 6),
    multistat("Avg GPU Util %", "avg({p}gpu_util)", 12, 5, 6, "percent"),
    multistat("Jobs Running", "{p}jobs_running", 18, 5, 6),
    # ── trends ─────────────────────────────────────────────────────────
    trend("Wall Power", "sum({p}node_sys_power_watts)", 0, 10, 12, "watt", stack=True),
    trend("Avg GPU Utilization", "avg({p}gpu_util)", 12, 10, 12, "percent"),
    trend("GPUs Allocated", "sum({p}gpus_allocated)", 0, 18, 8),
    trend("Jobs Running", "{p}jobs_running", 8, 18, 8),
    trend("Jobs Pending", "{p}jobs_pending", 16, 18, 8),
]

dash = {
    "__inputs": [{"name": "DS_PROMETHEUS", "label": "Prometheus",
                  "description": "Prometheus data source", "type": "datasource",
                  "pluginId": "prometheus", "pluginName": "Prometheus"}],
    "uid": "sgpu-overview",
    "title": "sgpu - All Clusters Overview",
    "tags": ["ai-master", "master", "overview"],
    "timezone": "browser",
    "refresh": "30s",
    "time": {"from": "now-6h", "to": "now"},
    "panels": panels,
    "templating": {"list": []},
    "schemaVersion": 39,
    "id": None,
}

with open("grafana/sgpu-overview.json", "w") as f:
    json.dump(dash, f, indent=2, ensure_ascii=False)
print(f"grafana/sgpu-overview.json: {len(panels)} panels")
