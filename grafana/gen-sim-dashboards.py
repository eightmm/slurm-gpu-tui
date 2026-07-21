#!/usr/bin/env python3
"""Regenerate the SIM-cluster dashboard pair from the local sources.

Rewrites: sgpu_* -> sim_sgpu_* everywhere, node_* -> sim_sgpu_master_*
inside panel exprs (the remote collector publishes its own host stats as
sgpu_master_* with node_exporter-compatible suffixes; the bridge adds the
sim_ prefix), cross-dashboard links, uids, titles, tags. The master
cluster has CPU-only nodes, so the "CPU ·" row ships expanded there.

Run from the repo root, then deploy with grafana/install.sh (or copy to
/var/lib/grafana/dashboards with ${DS_PROMETHEUS} -> prometheus and
__inputs stripped, as install.sh does).
"""
import json
import re

PAIRS = [
    ("grafana/sgpu-dashboard.json", "grafana/sgpu-sim-dashboard.json",
     "sgpu-sim-ops", "sgpu master (10.10.0.100) - SLURM GPU Operations"),
    ("grafana/sgpu-node-detail.json", "grafana/sgpu-sim-node-detail.json",
     "sgpu-sim-node-detail", "sgpu master (10.10.0.100) - Node Detail"),
]

# node_exporter metric names get the prefix only inside exprs — blanket
# string replace would also hit JSON keys and label matchers
_NODE_METRIC_RE = re.compile(r"\bnode_(?=[a-z])")


def _rewrite_exprs(panel: dict) -> None:
    for t in panel.get("targets", []):
        if t.get("expr"):
            t["expr"] = _NODE_METRIC_RE.sub("sim_sgpu_master_", t["expr"])
    for sub in panel.get("panels", []):
        _rewrite_exprs(sub)


def main() -> None:
    for src, dst, uid, title in PAIRS:
        s = open(src).read()
        s = s.replace("sgpu_", "sim_sgpu_")
        s = s.replace("sgpu-node-detail", "sgpu-sim-node-detail")
        s = s.replace("sgpu-slurm-gpu-ops", "sgpu-sim-ops")
        d = json.loads(s)
        d["uid"] = uid
        d["title"] = title
        d["tags"] = sorted(set(d.get("tags", [])) - {"ai-master"} | {"master", "sim"})
        for p in d.get("panels", []):
            _rewrite_exprs(p)
        # the sim master exposes no hwmon power sensor — drop dead panels
        # rather than shipping a permanent "No data" stat
        d["panels"] = [p for p in d["panels"]
                       if not any("hwmon_power" in (t.get("expr") or "")
                                  for t in p.get("targets", []))]
        for i, p in enumerate(d.get("panels", [])):
            if p.get("type") == "row" and p["title"].startswith("CPU ·") and p.get("collapsed"):
                inner = p.pop("panels")
                p["collapsed"] = False
                p["panels"] = []
                d["panels"][i + 1:i + 1] = inner
                break
        json.dump(d, open(dst, "w"), indent=2, ensure_ascii=False)
        print(f"{dst}: uid={uid}")


if __name__ == "__main__":
    main()
