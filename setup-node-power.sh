#!/usr/bin/env bash
# One-shot: make every SLURM compute node report power to sgpu.
#
#   sudo ./setup-node-power.sh            # probe all nodes, fix what's fixable
#   sudo ./setup-node-power.sh --check    # report only, change nothing
#
# What it fixes per node (root → node ssh, like push-mode installs):
#   BMC wall power  — installs ipmitool (deb fetched HERE and pushed over
#                     ssh: compute nodes often have no internet), loads
#                     ipmi_devintf/ipmi_si, persists them in modules-load.d
#   RAPL CPU power  — loads intel_rapl_common and persists it
#
# What it can NOT fix (reported as "hw-limit"):
#   nodes without a BMC (/dev/ipmi0 never appears), and RAPL on AMD CPUs
#   with kernels < 5.11 — wall power still covers those nodes.
#
# Idempotent: healthy nodes are probed and left untouched. Verify with
# `sgpu doctor` (power telemetry line) a minute after running.
set -uo pipefail

CHECK=0
[ "${1:-}" = "--check" ] && CHECK=1

command -v sinfo >/dev/null 2>&1 || { echo "sinfo not found — run on the SLURM master"; exit 1; }
[ "$(id -u)" -eq 0 ] || echo "WARN: not root — BMC probes need root on the nodes; results will show read-fail" >&2
NODES=$(sinfo -h -N -o "%N" | sort -u)
SSH="ssh -o BatchMode=yes -o ConnectTimeout=5"

DEB_DIR=$(mktemp -d)
trap 'rm -rf "$DEB_DIR"' EXIT
_fetch_deb() {  # once, on first node that needs it
    ls "$DEB_DIR"/ipmitool_*.deb >/dev/null 2>&1 && return 0
    (cd "$DEB_DIR" && apt-get download ipmitool >/dev/null 2>&1) \
        && ls "$DEB_DIR"/ipmitool_*.deb >/dev/null 2>&1
}

PROBE='
    bmc=no-dev; rapl=none
    [ -e /dev/ipmi0 ] || modprobe ipmi_devintf ipmi_si 2>/dev/null
    if [ -e /dev/ipmi0 ]; then
        if ! command -v ipmitool >/dev/null 2>&1; then bmc=no-tool
        elif ipmitool dcmi power reading >/dev/null 2>&1; then bmc=ok
        else bmc=read-fail; fi
    fi
    modprobe intel_rapl_common 2>/dev/null
    ls /sys/class/powercap 2>/dev/null | grep -q "^intel-rapl" && rapl=ok
    echo "$bmc $rapl"
'

fixed=0; broken=0
printf "%-12s %-10s %-8s %s\n" "node" "wall(BMC)" "rapl" "action"
for n in $NODES; do
    out=$($SSH "$n" "$PROBE" 2>/dev/null) || { printf "%-12s %-10s %-8s %s\n" "$n" "-" "-" "unreachable"; broken=$((broken+1)); continue; }
    bmc=${out% *}; rapl=${out#* }
    action="-"
    if [ "$bmc" = "no-tool" ] && [ "$CHECK" = 0 ]; then
        if _fetch_deb && scp -q "$DEB_DIR"/ipmitool_*.deb "$n:/tmp/"; then
            $SSH "$n" 'dpkg -i /tmp/ipmitool_*.deb >/dev/null 2>&1; rm -f /tmp/ipmitool_*.deb' 2>/dev/null
            action="installed ipmitool"
        else
            action="deb fetch/copy failed"
        fi
    fi
    if [ "$CHECK" = 0 ]; then
        # persist modules so a reboot doesn't silently lose telemetry
        $SSH "$n" 'printf "ipmi_devintf\nipmi_si\nintel_rapl_common\n" > /etc/modules-load.d/sgpu-power.conf' 2>/dev/null
        out=$($SSH "$n" "$PROBE" 2>/dev/null) && { bmc=${out% *}; rapl=${out#* }; }
    fi
    [ "$bmc" = "no-dev" ] && bmc="hw-limit"
    [ "$rapl" = "none" ] && rapl="hw-limit"
    [ "$action" = "installed ipmitool" ] && fixed=$((fixed+1))
    [ "$bmc" != "ok" ] && broken=$((broken+1))
    printf "%-12s %-10s %-8s %s\n" "$n" "$bmc" "$rapl" "$action"
done

echo ""
echo "fixed: $fixed · not reporting wall power: $broken"
echo "hw-limit = no BMC hardware, or AMD CPU on kernel <5.11 (RAPL) — nothing to install."
echo "verify in ~1 min:  sgpu doctor   (power telemetry line)"
