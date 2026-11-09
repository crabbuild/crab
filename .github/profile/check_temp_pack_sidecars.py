import hashlib, json, os, pathlib, shutil, subprocess, sys
root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(exist_ok=False)
git = shutil.which('git')
assert git
p = root / 'fixture'
p.mkdir()
repo = p / 'source'
env = os.environ | {'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': os.devnull}
def run(args, **kwargs):
    output = subprocess.check_output([str(x) for x in args], env=kwargs.pop('env', env), stderr=subprocess.PIPE, **kwargs)
    return (output if isinstance(output, str) else output.decode()).strip()
run([git, 'init', '--initial-branch=main', repo])
(repo / 'payload.bin').write_bytes(b'base')
run([git, '-C', repo, 'add', 'payload.bin'])
run([git, '-C', repo, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'base'])
parent = run([git, '-C', repo, 'rev-parse', 'HEAD'])
(repo / 'payload.bin').write_bytes(hashlib.shake_256(b'sidecar-fixture').digest(4096))
run([git, '-C', repo, 'add', 'payload.bin'])
run([git, '-C', repo, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'update'])
head = run([git, '-C', repo, 'rev-parse', 'HEAD'])
records = {}
for variant, value in [('default', 'true'), ('omit-unused-reverse', 'false')]:
    d = p / variant
    d.mkdir()
    traces = d / 'trace'
    traces.mkdir()
    trace_env = env | {'GIT_TRACE2_EVENT': str(traces)}
    cmd = [git, '--no-replace-objects', '--git-dir', repo / '.git', '--shallow-file', '', '-c', 'pack.writeReverseIndex=' + value,
           'pack-objects', '--quiet', '--revs', '--max-pack-size=2040109466', d / 'pack']
    oid = run(cmd, input=head + '\n^' + parent + '\n', text=True, env=trace_env)
    pack = d / f'pack-{oid}.pack'
    index = d / f'pack-{oid}.idx'
    assert pack.exists() and index.exists()
    rev = d / f'pack-{oid}.rev'
    assert rev.exists() == (value == 'true')
    verified = run([git, '--git-dir', repo / '.git', 'index-pack', '--fsck-objects', '-o', d / 'verified.idx', pack])
    assert verified == oid
    events = [json.loads(line) for f in traces.iterdir() for line in f.read_text().splitlines()]
    sync_counts = {e['name']: e['count'] for e in events if e.get('event') == 'counter' and e.get('category') == 'fsync'}
    records[variant] = dict(git_sha1=oid, pack_sha256=hashlib.sha256(pack.read_bytes()).hexdigest(),
        idx_sha256=hashlib.sha256(index.read_bytes()).hexdigest(),
        independently_verified_idx_sha256=hashlib.sha256((d / 'verified.idx').read_bytes()).hexdigest(),
        reverse_index_present=rev.exists(), sync_counts=sync_counts, fsck_objects='pass')
for key in ['git_sha1', 'pack_sha256', 'idx_sha256', 'independently_verified_idx_sha256']:
    assert records['default'][key] == records['omit-unused-reverse'][key], key
assert records['default']['idx_sha256'] == records['default']['independently_verified_idx_sha256']
assert set(records['default']['sync_counts']) == set(records['omit-unused-reverse']['sync_counts'])
assert len(records['default']['sync_counts']) == 1
method = next(iter(records['default']['sync_counts']))
assert method in {'hardware-flush', 'writeout-only'}
assert records['default']['sync_counts'][method] == 3 and records['omit-unused-reverse']['sync_counts'][method] == 2
run([git, '-C', repo, 'fsck', '--strict', '--full'])
proof = dict(scope='Component byte/durability boundary only, generated localGitfixture. No E2E speedup claim. Rawfixtureandtracesretained.',
    git=run([git, '--version']), records=records, local_flush_method=method,
    platform_scope='Per-platform component proof; this measures sync counts and byte identity, not speedup.',
    fsync_policy='No core.fsync/core.fsyncMethod modification. .pack and .idx exactbytes and independentfsck retained; only unused optional .rev omitted.',
    upstream='Git2.56pack.writeReverseIndex clearsWRITE_REV; pack-write.c skipsoptional.rev and itsownmetadatafsync, pack/indexfsyncunchanged.')
(root / 'verified-local-byte-contract.json').write_text(json.dumps(proof, indent=2) + '\n')
print(json.dumps(proof, indent=2))
