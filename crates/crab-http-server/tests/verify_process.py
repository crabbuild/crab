#!/usr/bin/env python3
"""Verify real server startup, graceful shutdown, and restart against native Git.

Supply an already bootstrapped deployment and pass verify_live.py arguments
after --. Logs and reports belong in a private directory on the workspace volume.
Storage credentials come from the environment and are never added to reports.
"""
import argparse
import json
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server-binary', required=True)
    parser.add_argument('--config', required=True)
    parser.add_argument('--url', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--restarts', type=int, default=1)
    parser.add_argument('checks', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if not 0 <= args.restarts <= 10:
        parser.error('--restarts must be between 0 and 10')
    checks = args.checks[1:] if args.checks[:1] == ['--'] else args.checks
    if not checks:
        parser.error('pass verify_live.py arguments after --')
    output = Path(args.output)
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    verifier = Path(__file__).with_name('verify_live.py')
    reports = []
    for iteration in range(args.restarts + 1):
        report = {'iteration': iteration}
        started = time.monotonic()
        with (output / f'server-{iteration}.log').open('wb') as log:
            process = subprocess.Popen(
                [args.server_binary, '--config', args.config, 'serve'],
                stdout=log, stderr=log,
            )
            try:
                deadline = started + 60
                while True:
                    if process.poll() is not None:
                        raise RuntimeError(f'server exited before readiness: {process.returncode}')
                    try:
                        with urllib.request.urlopen(args.url.rstrip('/') + '/livez', timeout=1) as response:
                            if response.status == 200:
                                break
                    except (OSError, urllib.error.URLError):
                        pass
                    if time.monotonic() >= deadline:
                        raise TimeoutError('server did not become live within 60 seconds')
                    time.sleep(0.05)
                report['startup_ms'] = (time.monotonic() - started) * 1000
                with (output / f'checks-{iteration}.json').open('wb') as results:
                    subprocess.run(
                        ['python3', str(verifier), '--url', args.url, *checks],
                        stdout=results, check=True,
                    )
                if process.poll() is not None:
                    raise RuntimeError('server exited during verification')
            finally:
                shutdown = time.monotonic()
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=120)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise TimeoutError('server exceeded its graceful shutdown deadline')
                report['shutdown_ms'] = (time.monotonic() - shutdown) * 1000
                report['exit_code'] = process.returncode
        if process.returncode != 0:
            raise RuntimeError(f'server exited unsuccessfully: {process.returncode}')
        reports.append(report)
        (output / 'process.json').write_text(json.dumps(reports, indent=2))
    print(json.dumps(reports, indent=2))


if __name__ == '__main__':
    main()
