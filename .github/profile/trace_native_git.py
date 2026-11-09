#!/usr/bin/env python3
"""Untimed diagnostic launcher; preserve native and remote-helper argv identity."""
import json
import os
from pathlib import Path
import sys
import time
import uuid

binary = Path(os.environ['TRACE_CRAB_REAL_BIN']).resolve(strict=True)
args = sys.argv[1:]
env = os.environ.copy()
if args and args[0] == 'push' and Path(sys.argv[0]).name != 'git-remote-crab':
    directory = Path(os.environ['TRACE_CRAB_OUTPUT']) / uuid.uuid4().hex
    directory.mkdir(parents=True)
    events = directory / 'git-events'
    events.mkdir()
    (directory / 'launch.json').write_text(json.dumps({
        'argv': args, 'cwd': str(Path.cwd()), 'pid': os.getpid(),
        'started_unix_ns': time.time_ns(),
        'purpose': 'Git child command inventory; instrumented timing is not performance evidence',
    }, indent=2) + '\n')
    env['GIT_TRACE2_EVENT'] = str(events)
# Git selects the helper by argv[0], even though this launcher execs another file.
# Preserve that identity so cold fetch/fsck exercises the normal wire path.
argv0 = sys.argv[0] if Path(sys.argv[0]).name == 'git-remote-crab' else str(binary)
os.execve(binary, [argv0, *args], env)
