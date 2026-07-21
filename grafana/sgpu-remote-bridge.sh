#!/usr/bin/env bash
# Mirror a remote sgpu cluster's Prometheus textfile into the local
# node_exporter textfile directory under a metric-name prefix, so one
# Grafana/Prometheus can watch several clusters without series collisions
# (and without the local sgpu_* alert rules ever matching remote series).
#
#   SGPU_BRIDGE_REMOTE       ssh target                (default sim@10.10.0.100)
#   SGPU_BRIDGE_REMOTE_FILE  remote metrics path       (default /tmp/slurm-gpu-tui/metrics.prom)
#   SGPU_BRIDGE_PREFIX       metric name prefix        (default sim_)
#   SGPU_BRIDGE_OUT          output .prom              (default /tmp/slurm-gpu-tui/<prefix>sgpu.prom)
#
# On fetch failure the data series are dropped (Prometheus marks them stale)
# and only <prefix>sgpu_bridge_up 0 remains — dashboards go "No data"
# instead of silently freezing on the last good numbers.
set -uo pipefail

REMOTE="${SGPU_BRIDGE_REMOTE:-sim@10.10.0.100}"
REMOTE_FILE="${SGPU_BRIDGE_REMOTE_FILE:-/tmp/slurm-gpu-tui/metrics.prom}"
PREFIX="${SGPU_BRIDGE_PREFIX:-sim_}"
OUT="${SGPU_BRIDGE_OUT:-/tmp/slurm-gpu-tui/${PREFIX}sgpu.prom}"

# temp name lacks the .prom suffix, so node_exporter never scrapes a half-written file
TMP="$(mktemp "${OUT}.XXXXXX")" || exit 1
trap 'rm -f "$TMP"' EXIT

up=0
if ssh -o BatchMode=yes -o ConnectTimeout=5 "$REMOTE" "cat '$REMOTE_FILE'" 2>/dev/null \
        | sed -E "s/^sgpu_/${PREFIX}sgpu_/; s/^# (HELP|TYPE) sgpu_/# \1 ${PREFIX}sgpu_/" >"$TMP" \
        && grep -q "^${PREFIX}sgpu_" "$TMP"; then
    up=1
else
    : >"$TMP"
fi
{
    printf '# HELP %ssgpu_bridge_up Remote sgpu fetch succeeded (1) or failed (0)\n' "$PREFIX"
    printf '# TYPE %ssgpu_bridge_up gauge\n' "$PREFIX"
    printf '%ssgpu_bridge_up %d\n' "$PREFIX" "$up"
} >>"$TMP"
chmod 644 "$TMP"
mv "$TMP" "$OUT"
