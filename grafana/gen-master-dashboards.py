#!/usr/bin/env python3
"""Regenerate the bridged-cluster dashboard pair from the
local ai-master sources.

Rewrites: sgpu_* -> master_sgpu_* everywhere, node_* -> master_sgpu_master_*
inside panel exprs (the remote collector publishes its own host stats as
sgpu_master_* with node_exporter-compatible suffixes; the bridge adds the
master_ prefix), cross-dashboard links, uids, titles, tags, and the disk
table's mountpoints (that cluster has /appl instead of /data1). The master
cluster has CPU-only nodes, so the "CPU ·" row ships expanded there.

Run from the repo root, then deploy with grafana/install.sh (or copy to
/var/lib/grafana/dashboards with ${DS_PROMETHEUS} -> prometheus and
__inputs stripped, as install.sh does).
"""
import json
import re

PAIRS = [
    ("grafana/sgpu-dashboard.json", "grafana/sgpu-master-dashboard.json",
     "sgpu-master-ops", "sgpu master - SLURM GPU Operations"),
    ("grafana/sgpu-node-detail.json", "grafana/sgpu-master-node-detail.json",
     "sgpu-master-node-detail", "sgpu master - Node Detail"),
]

# node_exporter metric names get the prefix only inside exprs — blanket
# string replace would also hit JSON keys and label matchers
_NODE_METRIC_RE = re.compile(r"\bnode_(?=[a-z])")


def _rewrite_exprs(panel: dict) -> None:
    for t in panel.get("targets", []):
        if t.get("expr"):
            t["expr"] = _NODE_METRIC_RE.sub("master_sgpu_master_", t["expr"])
            t["expr"] = t["expr"].replace("/data1", "/appl")
    for sub in panel.get("panels", []):
        _rewrite_exprs(sub)


def main() -> None:
    for src, dst, uid, title in PAIRS:
        s = open(src).read()
        s = s.replace("sgpu_", "master_sgpu_")
        s = s.replace("sgpu-node-detail", "sgpu-master-node-detail")
        s = s.replace("sgpu-slurm-gpu-ops", "sgpu-master-ops")
        d = json.loads(s)
        d["uid"] = uid
        d["title"] = title
        d["tags"] = sorted(set(d.get("tags", [])) - {"ai-master"} | {"master"})
        for p in d.get("panels", []):
            _rewrite_exprs(p)
        # the master cluster's master exposes no hwmon power sensor — drop
        # dead panels rather than shipping a permanent "No data" stat
        d["panels"] = [p for p in d["panels"]
                       if not any("hwmon_power" in (t.get("expr") or "")
                                  for t in p.get("targets", []))]
        json.dump(d, open(dst, "w"), indent=2, ensure_ascii=False)
        print(f"{dst}: uid={uid}")


if __name__ == "__main__":
    main()
