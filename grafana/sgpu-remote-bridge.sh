#!/usr/bin/env bash
# Mirror a remote sgpu cluster's Prometheus textfile into the local
# node_exporter textfile directory under a metric-name prefix, so one
# Grafana/Prometheus can watch several clusters without series collisions
# (and without the local sgpu_* alert rules ever matching remote series).
#
# Newer sgpu collectors publish their own host's stats as sgpu_master_*
# in metrics.prom, which the prefix rename above covers for free. For
# remotes still on an older sgpu, fall back to sampling the master's
# /proc//sys over ssh into the same <prefix>sgpu_master_* names, so the
# dashboard's "master (login/collector node)" row works either way.
#
#   SGPU_BRIDGE_REMOTE       ssh target                (default sim@10.10.0.100)
#   SGPU_BRIDGE_REMOTE_FILE  remote metrics path       (default /tmp/slurm-gpu-tui/metrics.prom)
#   SGPU_BRIDGE_PREFIX       metric name prefix        (default master_)
#   SGPU_BRIDGE_OUT          output .prom              (default /tmp/slurm-gpu-tui/<prefix>sgpu.prom)
#
# On fetch failure the data series are dropped (Prometheus marks them stale)
# and only <prefix>sgpu_bridge_up 0 remains — dashboards go "No data"
# instead of silently freezing on the last good numbers.
set -uo pipefail

REMOTE="${SGPU_BRIDGE_REMOTE:-sim@10.10.0.100}"
REMOTE_FILE="${SGPU_BRIDGE_REMOTE_FILE:-/tmp/slurm-gpu-tui/metrics.prom}"
PREFIX="${SGPU_BRIDGE_PREFIX:-master_}"
OUT="${SGPU_BRIDGE_OUT:-/tmp/slurm-gpu-tui/${PREFIX}sgpu.prom}"

SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=5)

# temp name lacks the .prom suffix, so node_exporter never scrapes a half-written file
TMP="$(mktemp "${OUT}.XXXXXX")" || exit 1
trap 'rm -f "$TMP"' EXIT

up=0
if ssh "${SSH_OPTS[@]}" "$REMOTE" "cat '$REMOTE_FILE'" 2>/dev/null \
        | sed -E "s/^sgpu_/${PREFIX}sgpu_/; s/^# (HELP|TYPE) sgpu_/# \1 ${PREFIX}sgpu_/" >"$TMP" \
        && grep -q "^${PREFIX}sgpu_" "$TMP"; then
    up=1
else
    : >"$TMP"
fi

# fallback master host stats for remotes on an sgpu without sgpu_master_*
# (only the metrics the dashboard's master row uses)
if ! grep -q "^${PREFIX}sgpu_master_" "$TMP"; then
ssh "${SSH_OPTS[@]}" "$REMOTE" /bin/sh 2>/dev/null <<'RSH' | sed "s/^node_/${PREFIX}sgpu_master_/" >>"$TMP"
awk '/^cpu[0-9]+ /{printf "node_cpu_seconds_total{cpu=\"%s\",mode=\"idle\"} %.2f\n", substr($1,4), $5/100}
     /^btime /{print "node_boot_time_seconds " $2}' /proc/stat
awk '/^MemTotal:/{print "node_memory_MemTotal_bytes " $2*1024}
     /^MemAvailable:/{print "node_memory_MemAvailable_bytes " $2*1024}' /proc/meminfo
awk '{print "node_load1 " $1}' /proc/loadavg
for m in / /home /data1; do
    df -B1 -P "$m" 2>/dev/null | awk -v m="$m" 'NR==2 && $6==m {
        print "node_filesystem_size_bytes{mountpoint=\"" m "\"} " $2
        print "node_filesystem_avail_bytes{mountpoint=\"" m "\"} " $4}'
done
awk 'NR>2 {sub(/^ */,""); split($0,a,":"); dev=a[1]; split(a[2],f," ")
     if (dev=="lo") next
     print "node_network_receive_bytes_total{device=\"" dev "\"} " f[1]
     print "node_network_transmit_bytes_total{device=\"" dev "\"} " f[9]}' /proc/net/dev
awk '$3 ~ /^(sd[a-z]+|vd[a-z]+|nvme[0-9]+n[0-9]+)$/ {
     print "node_disk_read_bytes_total{device=\"" $3 "\"} " $6*512
     print "node_disk_written_bytes_total{device=\"" $3 "\"} " $10*512}' /proc/diskstats
for h in /sys/class/hwmon/hwmon*; do
    [ -r "$h/name" ] || continue
    [ "$(cat "$h/name")" = "coretemp" ] || continue
    for t in "$h"/temp*_input; do
        [ -r "$t" ] || continue
        s=${t##*/}; s=${s%_input}
        awk -v s="$s" '{printf "node_hwmon_temp_celsius{chip=\"platform_coretemp.0\",sensor=\"%s\"} %.1f\n", s, $1/1000}' "$t"
    done
done
RSH
fi

{
    printf '# HELP %ssgpu_bridge_up Remote sgpu fetch succeeded (1) or failed (0)\n' "$PREFIX"
    printf '# TYPE %ssgpu_bridge_up gauge\n' "$PREFIX"
    printf '%ssgpu_bridge_up %d\n' "$PREFIX" "$up"
} >>"$TMP"
chmod 644 "$TMP"
mv "$TMP" "$OUT"
