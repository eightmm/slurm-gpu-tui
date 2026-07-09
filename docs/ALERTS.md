# Slack Alerts

The collector can push cluster alerts to Slack. Config lives in
`~/.sgpu/webhook.json` (root's `/root/.sgpu/webhook.json` for a system
service) and is **hot-reloaded** — edit and save, no restart needed. The
installer offers to set it up; `sgpu doctor` shows the active mode.

## Setup

Two delivery modes:

- **Incoming webhook** (simplest): create a Slack incoming webhook, paste the
  `https://hooks.slack.com/services/...` URL when the installer asks. Alerts
  post as individual messages.
- **Bot mode** (recommended): create a Slack app with the `chat:write` scope,
  install it, invite it to the channel (`/invite @your-bot`), and give the
  installer the bot token (`xoxb-...`) and channel. Alerts then post as
  replies under **one parent message per day** (`📅 GPU cluster alerts — date`),
  keeping the channel tidy. Incoming webhooks cannot thread.

Non-interactive: `SGPU_WEBHOOK_URL`, `SGPU_WEBHOOK_SENDER`, `SGPU_WEBHOOK_LANG`,
`SGPU_SLACK_BOT_TOKEN`, `SGPU_SLACK_CHANNEL` skip the matching prompts.

## Config keys (`~/.sgpu/webhook.json`)

| Key | Default | Meaning |
|-----|---------|---------|
| `url` | — | Slack incoming-webhook URL |
| `bot_token` / `channel` | — | Bot token + channel → daily-thread grouping |
| `sender_name` | `AI-master` | Identity shown in every alert (set per cluster to tell them apart) |
| `lang` | `en` | Alert language: `en` or `ko` |
| `node_health` | `true` | Node down/recovered, judged by **SLURM state** (not sgpu's own SSH/collection errors) |
| `down_grace_sec` | `180` | Down must persist this long before alerting (rides out cold starts / blips) |
| `collect_alert` | `true` | A GPU node that was reporting goes blind (dead agent / hung node) while SLURM still shows it up |
| `collect_grace_sec` | `600` | Blind must persist this long (longer than the agent auto-repair cycle) |
| `waste_alert_hours` | `2` | GPU idle/parked ≥ N hours (`0` = off) |
| `rogue_alert` | `true` | GPU used outside SLURM |
| `temp_alert_c` | `0` | GPU temperature ≥ N °C (`0` = off; ~90 typical) |
| `ecc_alert` | `true` | Uncorrectable ECC errors — silent hardware failure; alert carries UUID / PCI bus / serial for RMA |
| `job_done_users` | `[]` | Notify when these users' jobs finish |
| `free_gpus_min` | `0` | Notify when free-GPU count reaches N (`0` = off) |

Repeated conditions are debounced (30 min for events, 6 h for standing
conditions like waste/temp/ECC). Node-health and collection alerts are for
**GPU nodes and SLURM state only** — SSH-pull clusters routinely can't reach
CPU/GPU-less nodes, and those collection failures never raise a false "down".

The TUI shows the same job/node transitions as toasts while it's open, so you
don't need Slack for at-the-terminal awareness.
