import hashlib, json, os, pathlib, random, subprocess, sys, time, uuid

backend = sys.argv[1]
salt = uuid.uuid4().hex
work = pathlib.Path(os.environ['TMPDIR']) / ('mapped-provider-' + backend + '-' + salt[:10])
helper_controls = False
out = pathlib.Path('/e2e') / (backend + ('-helper' if helper_controls else '-completed'))
seed = work / 'seed'
mount = pathlib.Path('/mnt/mapped')
seed.mkdir(parents=True)
mount.mkdir(parents=True)
out.mkdir(parents=True, exist_ok=True)
os.environ['PATH'] = '/e2e/bin:' + os.environ['PATH']
os.environ['CRAB_CACHE_DIR'] = str(work / 'cache')
os.environ['GIT_TERMINAL_PROMPT'] = '0'
remote = 'crab://crab/mount-large/mapped-' + backend + '-' + salt
sequence = 0

def run(args, label, env=None):
    global sequence
    sequence += 1
    if helper_controls and args[0] == 'crab' and (args[1] == 'unmount' or (args[1] == 'mount' and args[2] in ('refresh', 'switch', 'commit', 'status'))):
        args = ['crab-fuse-mount', *args[1:]]
    result = subprocess.run(args, cwd=seed, env=env, capture_output=True, timeout=180)
    (out / f'{sequence:02d}-{label}.stdout').write_bytes(result.stdout)
    (out / f'{sequence:02d}-{label}.stderr').write_bytes(result.stderr)
    if result.returncode:
        raise RuntimeError(f'{label} failed with {result.returncode}: {result.stderr[-1800:].decode(errors="replace")}')
    return result.stdout

def git(*args):
    return run(['git', *args], 'git-' + args[0])

def payload(label):
    return random.Random(salt + label).randbytes(4 * 1024 * 1024)

def publish(label, branch):
    (seed / 'model.bin').write_bytes(payload(label))
    run(['crab', 'add', 'model.bin'], label + '-add')
    git('add', 'crab.toml', '.gitattributes')
    git('commit', '-m', label)
    run(['crab', 'push', 'origin', 'HEAD:refs/heads/' + branch], label + '-push')

run(['git', 'config', '--global', 'user.name', 'VFS qualification'], 'global-name')
run(['git', 'config', '--global', 'user.email', 'audit@example.invalid'], 'global-email')
git('init', '-b', 'main')
git('config', 'user.name', 'VFS qualification')
git('config', 'user.email', 'audit@example.invalid')
run(['crab', 'init', remote], 'init')
run(['crab', 'track', '*.bin'], 'track')
publish('seed', 'main')
results = []
client_code = '''import mmap, os, sys
fd = os.open(sys.argv[1], os.O_RDWR)
mapping = mmap.mmap(fd, 4096)
mapping[0] ^= 255
sys.stdout.buffer.write(b'1')
sys.stdout.buffer.flush()
sys.stdin.buffer.read(1)
mapping.close()
os.close(fd)
'''
client = None
mounted = False
try:
    run(['crab', 'mount', '--repo', remote, '--mountpoint', str(mount), '--ref', 'main', '--backend', backend, '--no-refresh'], 'mount')
    mounted = True
    status = json.loads(run(['crab', 'mount', 'status', '--mountpoint', str(mount), '--live-only', '--json'], 'live-status'))
    listing = json.loads(run(['crab', 'mount', 'list', '--json'], 'live-list'))
    assert str(mount) in json.dumps(listing), listing
    reset_path = mount / 'control-reset.tmp'
    reset_path.write_bytes(b'owned reset probe')
    diff = json.loads(run(['crab', 'mount', 'diff', '--mountpoint', str(mount), '--json'], 'live-diff'))
    assert 'control-reset.tmp' in json.dumps(diff), diff
    run(['crab', 'mount', 'reset', '--mountpoint', str(mount), '--overlay', '--yes', '--json'], 'live-reset')
    assert not reset_path.exists(), 'reset returned before namespace update'
    for action in ('refresh', 'switch', 'automatic'):
        if action == 'automatic':
            run(['crab', 'unmount', '--mountpoint', str(mount)], 'remount-unmount')
            git('fetch', 'origin', 'other')
            git('reset', '--hard', 'FETCH_HEAD')
            run(['crab', 'mount', '--repo', remote, '--mountpoint', str(mount), '--ref', 'other', '--backend', backend], 'mount-automatic')
        path = mount / 'model.bin'
        expected = bytearray(path.read_bytes())
        assert len(expected) == 4 * 1024 * 1024
        expected[0] ^= 255
        client = subprocess.Popen(['python3', '-c', client_code, str(path)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        assert client.stdout.read(1) == b'1'
        branch = 'main' if action == 'refresh' else 'other'
        if action == 'switch':
            git('switch', '-c', branch)
        publish('diverging-' + action, branch)
        control = ['crab', 'mount', action, '--mountpoint', str(mount)]
        if action == 'switch':
            control += ['--ref', branch]
        if action == 'automatic':
            expected_head = git('rev-parse', 'HEAD').decode().strip()
            deadline = time.monotonic() + 75
            while True:
                status = json.loads(run(['crab', 'mount', 'status', '--mountpoint', str(mount), '--live-only', '--json'], 'automatic-status'))
                status = status.get('data', status)
                if status.get('head_oid') == expected_head:
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError(f'automatic refresh did not expose {expected_head}: {status}')
                time.sleep(2)
        else:
            run(control, action)
        run(['crab', 'mount', 'commit', '--mountpoint', str(mount), '--message', 'retain mapped ' + action, '--push', '--json'], action + '-commit-push')
        clone = work / ('clone-' + action)
        clone_env = dict(os.environ, CRAB_CACHE_DIR=str(work / ('clone-cache-' + action)))
        run(['crab', 'clone', remote, str(clone), '--eager', '--branch', branch], action + '-clone', clone_env)
        actual = (clone / 'model.bin').read_bytes()
        expected_hash = hashlib.sha256(expected).hexdigest()
        actual_hash = hashlib.sha256(actual).hexdigest()
        assert actual == expected, f'{action}: clone {actual_hash} differs from mapped edit {expected_hash}'
        client.stdin.write(b'1')
        client.stdin.flush()
        assert client.wait(timeout=30) == 0
        client = None
        results.append({'action': action, 'branch': branch, 'bytes': len(actual), 'expected_sha256': expected_hash, 'clone_sha256': actual_hash})
        print(json.dumps(results[-1]), flush=True)
finally:
    if client is not None:
        client.kill()
        client.wait(timeout=30)
    if mounted:
        run(['crab', 'unmount', '--mountpoint', str(mount)], 'unmount')
(out / 'result.json').write_text(json.dumps({'backend': backend, 'helper_controls': helper_controls, 'salt': salt, 'remote': remote, 'work': str(work), 'results': results}, indent=2) + '\n')
print('mapped_transition_provider_e2e=ok', flush=True)
