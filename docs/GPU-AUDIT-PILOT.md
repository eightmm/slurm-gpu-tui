# SGPU Capacity Audit — paid pilot specification

A **read-only GPU efficiency audit** for research labs, shared HPC clusters,
and small/medium AI infrastructure teams already using Slurm.

> Status: pilot offer / pricing hypothesis. No customers, contracts,
> revenue, savings or production deployments are asserted here.

## Deliverables

1. Reproducible 7–30 day usage report based on existing collector history.
2. Review of allocated and observed-busy GPU-hours, idle/parked waste
   indicators, coverage gaps, and prioritized next actions.
3. Optional modeled capacity value only with a **client-supplied** GPU-hour
   rate and reclaimable fraction. This is **not realized cash savings**.
4. Written remediation plan. Any Slurm/policy changes require client approval.

The open-source sgpu tool remains MIT licensed. Paid work means diagnosis,
secure integration, validation, support, and operator time—not paid access
to the free CLI.

## Customer-safe workflow

- Verify source locally with sgpu doctor.
- Generate local 30-day report: sgpu audit --days 30
- Model an explicit example: sgpu audit --days 30 --rate 2 --reclaimable 0.25 --currency USD
- Offline authorized input: sgpu audit --input /path/to/usage.json --format json
- Never request root credentials, SSH keys, scheduler logs, API tokens,
  internal workload names or individual job records in an initial discussion.
- Request only anonymized aggregate report outputs and coverage context.
- Per-user details are excluded by default; --include-users needs operator
  approval and must not be shared externally without permission.
- Confirm ownership and authority to commercialize any operational data.

## Pricing experiments — NOT validated prices

| Offer | Trial quote | Included | Not included |
| --- | ---: | --- | --- |
| Read-only diagnostic | $300 one-time | one cluster, coverage assessment, historical audit, review | system changes |
| Slurm/GPU observability setup | from $1,500 scoped | install/configure, monitoring integration, tests, handover | hardware, 24/7 SLA |
| Monthly optimization review | from $250/month | read-only reviews and recommendations | automated production changes |

These are **quotes to test**, not existing customers or demonstrated revenue.
Actual quotes depend on nodes, telemetry coverage, access and security needs.

## Acceptance criteria

- Audit operates offline from a supplied JSON fixture, without contacting nodes.
- Computations reuse the canonical per-day sacct/usage merge rule.
- Efficiency excludes unobserved allocation from its denominator.
- Telemetry freshness and uncertainty are displayed.
- Default output excludes usernames, job IDs, and node names.
- No Slurm settings, access controls, secrets, billing or live data are changed.
- Client approves any production deployment, export, or paid engagement.

## Sales experiment

- Identify ten qualified Slurm-cluster administrators or managed-HPC integrators
  using public evidence and a legitimate inquiry channel.
- Offer an aggregate-only audit, ask for a short discovery conversation.
- Personalize each inquiry; no scraped bulk email.
- Record replies, discovery calls, scoped quotes, paid pilots, delivery hours.
- If fewer than two genuine discovery calls after ten qualified approaches,
  revisit segment or positioning before investing in more features.

## Suggested outreach sentence

> I can help you establish a read-only baseline of GPU allocation, observed
> utilization and avoidable *capacity* in your existing Slurm cluster.
> The report uses aggregate data only; monetary impact is labeled a
> scenario and is not presented as guaranteed cash savings.

## Competitive position (November 2026)

- SchedMD (https://www.schedmd.com/) provides upstream Slurm support.
- Open OnDemand (https://github.com/OSC/ondemand) provides a browser HPC portal.
- NVIDIA Run:ai (https://www.nvidia.com/en-us/software/run-ai/) targets
  Kubernetes-native AI GPU orchestration; it is not a Slurm waste audit.

Sell a narrowly scoped operational service, not a new scheduler or
enterprise GPU-orchestration replacement.
