# Running in a Slurm batch job

Use a batch script to start your service and run `portable-opentunnel` in the
foreground. No Slurm plugin is needed. Adapt the service command, time limit,
and directory to your site:

```bash
#!/usr/bin/env bash
#SBATCH --job-name=my-service
#SBATCH --time=08:00:00
#SBATCH --nodes=1
#SBATCH --ntasks=1

set -euo pipefail

python3 ./serve.py --port 3000 &
exec ./portable-opentunnel 3000 --ephemeral
```

`exec` makes the wrapper the batch process, so a SIGTERM sent to that
process reaches the wrapper directly. The example relies on Slurm to clean up
the background service when the job ends; no shell is left to run an `EXIT`
trap. Check how your site signals and cleans up jobs.

- Set `--shutdown-timeout` below your site's grace period between SIGTERM and
  SIGKILL if you need ephemeral deletion to finish.
- `--ephemeral` always creates its own directory. Set `TMPDIR` to an existing
  parent on durable storage if you want to keep recovery credentials after
  node failure; the default temporary directory is often node-local.
- For a stable URL, omit `--ephemeral` and supply the required `--directory`.
  Do not share that directory between concurrent jobs.

The public URL appears on stdout once the bridge attaches; verify application
readiness separately. See the [operations guide](operations.md) for recovery.
