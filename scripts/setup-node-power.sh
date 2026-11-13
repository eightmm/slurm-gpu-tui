#!/usr/bin/env bash
# Inspect/configure power telemetry on this node only.
set -euo pipefail
APPLY=0
case "${1:---check}" in
    --check) ;;
    --apply) APPLY=1 ;;
    *) echo "usage: $0 --check | --apply (run locally on the node)" >&2; exit 2 ;;
esac
if [ "$APPLY" = 1 ]; then
    [ "$(id -u)" = 0 ] || { echo "--apply requires root on this node" >&2; exit 1; }
    if ! command -v ipmitool >/dev/null; then
        if command -v apt-get >/dev/null; then apt-get install -y ipmitool
        elif command -v dnf >/dev/null; then dnf install -y ipmitool
        elif command -v yum >/dev/null; then yum install -y ipmitool
        else echo "install ipmitool from this node's approved package source" >&2; exit 1
        fi
    fi
    for module in ipmi_devintf ipmi_si intel_rapl_common; do
        modprobe "$module" 2>/dev/null || true
    done
    [ ! -L /etc/modules-load.d/sgpu-power.conf ] || { echo "refusing symlinked module configuration" >&2; exit 1; }
    printf '# Managed by sgpu (node-local)\nipmi_devintf\nipmi_si\nintel_rapl_common\n' \
        > /etc/modules-load.d/sgpu-power.conf
fi
BMC=no-device
if [ -e /dev/ipmi0 ]; then
    if ! command -v ipmitool >/dev/null; then BMC=no-tool
    elif timeout 8 ipmitool dcmi power reading >/dev/null 2>&1; then BMC=ok
    else BMC=unreadable
    fi
fi
RAPL=unavailable
for entry in /sys/class/powercap/intel-rapl:*; do
    if [ -r "$entry/energy_uj" ]; then RAPL=ok; break; fi
done
printf 'local power: BMC=%s RAPL=%s\n' "$BMC" "$RAPL"
