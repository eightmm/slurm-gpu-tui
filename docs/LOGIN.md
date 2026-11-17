# Login summaries

`sgpu welcome` shows a short overview using only the published snapshot. It never
queries Slurm, probes nodes or starts a collector. Missing, invalid or more than
30-second-old data produces an unavailable message and a successful exit so a
collector problem does not prevent login.

```bash
sgpu welcome                       # Cluster and my jobs
sgpu welcome --scope cluster       # Public overview, safe for shared MOTD
sgpu welcome --scope personal      # Actual executing account's jobs only
sgpu welcome --width 60 --color never
```

Shared MOTD is cached across accounts. Use `--scope cluster` there; put personal
summaries in the user's interactive shell. The executing account comes from its
numeric identity, not `$USER`. Job details and logs are never printed.

## Optional shared MOTD

On a Linux controller where `/usr/local/bin/sgpu` is installed:

```bash
sudo install -m 0755 scripts/login-motd.sh /etc/update-motd.d/80-sgpu
```

Enable one cluster overview hook at a time to avoid duplicate screens. Preserve
existing site notices, quota warnings and SSH/job-adoption rules when replacing a
site hook. The example has a two-second timeout and a one-second kill grace. It
does not change SSH, PAM or scheduler policy. Hook installation is opt-in.

Optional `/etc/sgpu/welcome.json`:

```json
{
  "title": "Research cluster",
  "notice_file": "/etc/sgpu/notice",
  "guide": "/path/to/cluster-guide.md",
  "hints": ["Apps: module avail | module help NAME", "After a job: seff JOBID"]
}
```

Display configuration and notice files must be root-owned, world-readable,
regular files, at most 8 KiB, and not writable by other accounts. Symlinks are
rejected. Invalid settings produce a warning and the default display.
Up to three nonempty hints can preserve useful site commands without listing all
installed software. Keep policy hints conditional where administrators or other
accounts have exceptions; an SSH connection alone does not prove job adoption.

Notices show at most three active lines. Blank lines and `#` comments are skipped;
`[until 2027-01-31] Maintenance at 18:00` expires after the stated local date.
Colors follow the common `--color auto|always|never` policy. The MOTD example uses
colors unless `NO_COLOR` or `TERM=dumb` disables them. Rows are clipped to terminal
cell width, including Korean text; `--width` accepts 16–240 columns.

For an optional personal shell summary, guard it so commands, file transfers and
noninteractive shells remain quiet:

```sh
case $- in
  *i*) if [ -n "${SSH_CONNECTION:-}" ] && [ -t 1 ]; then
    timeout --kill-after=1 2 sgpu welcome --scope personal || :
  fi ;;
esac
```

The snapshot must exist locally at `SLURM_GPU_TUI_DATA_DIR/data.json`; matching
directory names on different hosts do not share data. Keep compute-node session
and job-limit notices in their existing site hooks until that host has a suitable
published snapshot. `sgpu`, `chkgpu --summary`, `chkqueue --mine` and `chkfail`
provide more detail on demand. Reported allocation never guarantees admission.
