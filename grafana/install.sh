#!/usr/bin/env bash
# Install the sgpu monitoring stack on the collector host (master):
#   node_exporter  — textfile collector for /tmp/slurm-gpu-tui/metrics.prom,
#                    bound to 127.0.0.1:9100
#   prometheus     — scrapes node_exporter, loads sgpu alert rules,
#                    bound to 127.0.0.1:9090
#   grafana        — provisioned datasource + sgpu dashboard, listens on
#                    0.0.0.0:3000 (login required, sign-up disabled)
#
# Idempotent: safe to re-run. Run as root (sudo grafana/install.sh).
#
# Options:
#   --no-grafana        skip Grafana entirely (node_exporter + prometheus only;
#                       use when Grafana runs elsewhere and scrapes this host)
#   SGPU_INSTALL_GRAFANA=0   same as --no-grafana
#
# Every unit gets Restart=always: site cron sweeps (pkill -f node/python/...)
# send SIGTERM and a clean exit would leave on-failure units dead. Do not
# run any of this stack in docker — the same sweeps match "docker".
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[ "$(id -u)" -eq 0 ] || { echo "run as root: sudo $0"; exit 1; }
DASH_OWNER="${SUDO_USER:-root}"

WITH_GRAFANA="${SGPU_INSTALL_GRAFANA:-1}"
for arg in "$@"; do
    case "$arg" in
        --no-grafana) WITH_GRAFANA=0 ;;
        *) echo "unknown option: $arg (supported: --no-grafana)"; exit 1 ;;
    esac
done

echo "== [1/6] apt packages (prometheus, node_exporter) =="
export DEBIAN_FRONTEND=noninteractive
apt-get install -y --no-install-recommends prometheus prometheus-node-exporter

if [ "$WITH_GRAFANA" = 1 ]; then
    echo "== [2/6] grafana apt repo + install =="
    if [ ! -f /etc/apt/keyrings/grafana.gpg ]; then
        mkdir -p /etc/apt/keyrings
        curl -fsSL https://apt.grafana.com/gpg.key | gpg --dearmor -o /etc/apt/keyrings/grafana.gpg
    fi
    echo "deb [signed-by=/etc/apt/keyrings/grafana.gpg] https://apt.grafana.com stable main" \
        > /etc/apt/sources.list.d/grafana.list
    apt-get update -o Dir::Etc::sourcelist=/etc/apt/sources.list.d/grafana.list \
        -o Dir::Etc::sourceparts=/dev/null -o APT::Get::List-Cleanup=0
    apt-get install -y grafana
else
    echo "== [2/6] grafana skipped (--no-grafana) =="
fi

echo "== [3/6] node_exporter: textfile collector, localhost only =="
# PrivateTmp=no: the distro unit's private /tmp would hide the metrics file
# (see docs/GRAFANA.md "PrivateTmp trap").
cat > /etc/default/prometheus-node-exporter <<'EOF'
ARGS="--collector.textfile.directory=/tmp/slurm-gpu-tui --web.listen-address=127.0.0.1:9100"
EOF
mkdir -p /etc/systemd/system/prometheus-node-exporter.service.d
cat > /etc/systemd/system/prometheus-node-exporter.service.d/sgpu.conf <<'EOF'
[Service]
PrivateTmp=no
Restart=always
RestartSec=10
EOF

echo "== [4/6] prometheus: scrape config + sgpu rules, localhost only =="
sed -i 's|^ARGS=.*|ARGS="--web.listen-address=127.0.0.1:9090"|' /etc/default/prometheus
mkdir -p /etc/prometheus/rules
install -m 644 "$REPO"/prometheus/*.yml /etc/prometheus/rules/
if [ -f /etc/prometheus/prometheus.yml ] && [ ! -f /etc/prometheus/prometheus.yml.dist ]; then
    cp /etc/prometheus/prometheus.yml /etc/prometheus/prometheus.yml.dist
fi
cat > /etc/prometheus/prometheus.yml <<'EOF'
global:
  scrape_interval: 15s
  evaluation_interval: 15s

rule_files:
  - /etc/prometheus/rules/*.yml

scrape_configs:
  - job_name: prometheus
    static_configs:
      - targets: ['localhost:9090']
  - job_name: node
    static_configs:
      - targets: ['localhost:9100']
EOF
promtool check config /etc/prometheus/prometheus.yml
mkdir -p /etc/systemd/system/prometheus.service.d
cat > /etc/systemd/system/prometheus.service.d/sgpu.conf <<'EOF'
[Service]
Restart=always
RestartSec=10
EOF

if [ "$WITH_GRAFANA" = 1 ]; then
echo "== [5/6] grafana: provisioning + hardening =="
mkdir -p /etc/systemd/system/grafana-server.service.d
cat > /etc/systemd/system/grafana-server.service.d/sgpu.conf <<'EOF'
[Service]
Restart=always
RestartSec=10
Environment=GF_USERS_ALLOW_SIGN_UP=false
EOF
cat > /etc/grafana/provisioning/datasources/sgpu.yaml <<'EOF'
apiVersion: 1
datasources:
  - name: Prometheus
    type: prometheus
    uid: prometheus
    access: proxy
    url: http://localhost:9090
    isDefault: true
EOF
cat > /etc/grafana/provisioning/dashboards/sgpu.yaml <<'EOF'
apiVersion: 1
providers:
  - name: sgpu
    folder: ''
    type: file
    allowUiUpdates: true
    options:
      path: /var/lib/grafana/dashboards
EOF
mkdir -p /var/lib/grafana/dashboards
# The repo dashboard is a UI export: fix the datasource input for file
# provisioning (import-style ${DS_PROMETHEUS} is not resolved there).
python3 - "$REPO/grafana/sgpu-dashboard.json" \
    /var/lib/grafana/dashboards/sgpu-dashboard.json <<'EOF'
import json, sys
src, dst = sys.argv[1], sys.argv[2]
d = json.loads(open(src).read().replace('${DS_PROMETHEUS}', 'prometheus'))
d.pop('__inputs', None)
d.pop('__requires', None)
d['id'] = None
json.dump(d, open(dst, 'w'), indent=2)
EOF
# Owner = the invoking user so dashboard iterations don't need sudo;
# grafana only needs read.
chown -R "$DASH_OWNER":grafana /var/lib/grafana/dashboards
chmod 755 /var/lib/grafana/dashboards
chmod 644 /var/lib/grafana/dashboards/sgpu-dashboard.json
fi

SERVICES=(prometheus-node-exporter prometheus)
[ "$WITH_GRAFANA" = 1 ] && SERVICES+=(grafana-server)

echo "== [6/6] start services =="
systemctl daemon-reload
systemctl enable --now "${SERVICES[@]}"
systemctl restart "${SERVICES[@]}"

echo "== verify =="
sleep 5
echo "--- binds (9090/9100 must be 127.0.0.1; 3000 open when grafana installed):"
ss -tln | grep -E ':(3000|9090|9100)\b'
echo "--- node_exporter sgpu metric count:"
curl -s http://127.0.0.1:9100/metrics | grep -c '^sgpu_' || echo "FAIL: no sgpu_ metrics"
if [ "$WITH_GRAFANA" = 1 ]; then
    echo "--- grafana health:"
    curl -s http://127.0.0.1:3000/api/health
    echo
fi
echo "--- prometheus query (may be empty until first scrape):"
curl -s 'http://127.0.0.1:9090/api/v1/query?query=sgpu_gpus_total' | grep -o '"status":"[a-z]*"'
systemctl is-active "${SERVICES[@]}"
if [ "$WITH_GRAFANA" = 1 ]; then
    echo "DONE — open http://<master>:3000 (login required; create Viewer accounts for the lab)"
else
    echo "DONE — prometheus bound to 127.0.0.1:9090 (no auth). An external"
    echo "Grafana needs a tunnel/reverse-proxy to reach it, or widen"
    echo "--web.listen-address in /etc/default/prometheus behind a firewall."
fi
