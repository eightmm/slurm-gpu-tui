#!/usr/bin/env bash
# Report swap usage on the current host: total, per-user totals, and the
# heaviest processes. Run it directly on a node (e.g. after `ssh gpu3`).
set -u

free -h | awk '/Swap/{print "SWAP: "$3" / "$2}'

echo "--- per-user swap ---"
for p in /proc/[0-9]*; do
    s=$(awk '/^VmSwap/{print $2}' "$p/status" 2>/dev/null)
    [ "${s:-0}" -gt 0 ] && echo "$(stat -c %U "$p") $s"
done | awk '{a[$1]+=$2} END{for(u in a) printf "%.1f GB\t%s\n", a[u]/1048576, u}' | sort -rn

echo "--- top procs (>500MB swap) ---"
for p in /proc/[0-9]*; do
    s=$(awk '/^VmSwap/{print $2}' "$p/status" 2>/dev/null)
    [ "${s:-0}" -gt 512000 ] && printf "%5dMB %-10s pid=%-8s %s\n" \
        $((s/1024)) "$(stat -c %U "$p")" "${p#/proc/}" \
        "$(tr '\0' ' ' < "$p/cmdline" | cut -c1-70)"
done | sort -rn
