#!/usr/bin/env python3
"""Compare a running Crab HTTP server with a read-only native Git oracle."""
import argparse
import hashlib
from http.cookiejar import MozillaCookieJar
import json
import statistics
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True, help='Server origin, e.g. http://127.0.0.1:8788')
    parser.add_argument('--repository', required=True, help='Configured owner/name')
    parser.add_argument('--source', required=True, help='Read-only qualification Git checkout')
    parser.add_argument('--revision', required=True, help='Uploaded commit OID')
    parser.add_argument('--directory', action='append', default=[''])
    parser.add_argument('--file', action='append')
    parser.add_argument('--blame', help='Optional ordinary text path for first-parent blame comparison')
    parser.add_argument('--cookies', help='Private Netscape cookie file from an authenticated session')
    args = parser.parse_args()
    jar = MozillaCookieJar(args.cookies)
    if args.cookies:
        jar.load(ignore_discard=True)
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
    samples = {}
    checks = {'tree_entries': 0, 'commits': 0, 'blobs': 0, 'diffs': 0, 'errors': 0}
    api = '/api/repos/' + '/'.join(urllib.parse.quote(part, safe='') for part in args.repository.split('/'))

    def git(*command):
        return subprocess.check_output(['git', '-C', args.source, *command])

    def http(path, status=200, method='GET', headers=None):
        request = urllib.request.Request(args.url.rstrip('/') + path, method=method, headers=headers or {})
        started = time.perf_counter()
        try:
            response = opener.open(request, timeout=40)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            body = response.read()
            assert response.status == status, (path, response.status, body[:300])
            if status == 200 and path.startswith(api):
                assert response.headers['Server-Timing'], path
                assert response.headers['X-Crab-Generation'], path
                assert response.headers['Cache-Control'] == 'no-store', path
                action = path.split('?', 1)[0].rsplit('/', 1)[-1]
                samples.setdefault(action, []).append((time.perf_counter() - started) * 1000)
            return body, response.headers

    def read(action, **params):
        params.setdefault('rev', args.revision)
        return json.loads(http(api + '/' + action + '?' + urllib.parse.urlencode(params))[0])

    def verify_content(content, revision, path):
        body = git('show', f'{revision}:{path}')
        oid = hashlib.sha1(b'blob ' + str(len(body)).encode() + b'\0' + body).hexdigest()
        assert content['oid'] == oid and content['size'] == len(body), path
        if content['text'] is not None:
            assert content['text'].encode() == body, path
        return body

    assert git('rev-parse', args.revision).decode().strip() == args.revision
    refs = read('refs')
    assert refs['head']['oid'] == args.revision
    for directory in args.directory:
        expected = git('ls-tree', '-z', args.revision + (':' + directory if directory else ''))
        expected = [record.split(b'\t', 1) for record in expected.split(b'\0') if record]
        items = []
        cursor = None
        while True:
            params = {'path': directory, 'limit': 7}
            if cursor:
                params['cursor'] = cursor
            page = read('tree', **params)
            assert page['commit'] == args.revision
            items.extend(page['items'])
            cursor = page['next']
            if not cursor:
                break
        actual = [(item['mode'].lstrip('0'), item['oid'], bytes.fromhex(item['path_hex']).rsplit(b'/', 1)[-1]) for item in items]
        oracle = [(meta.split()[0].decode().lstrip('0'), meta.split()[2].decode(), name) for meta, name in expected]
        assert actual == oracle, directory
        checks['tree_entries'] += len(items)
    page = read('commits', limit=10)
    assert [item['oid'] for item in page['items']] == git('rev-list', '--first-parent', '--max-count=10', args.revision).decode().splitlines()
    for commit in page['items']:
        raw = git('cat-file', 'commit', commit['oid'])
        assert bytes.fromhex(commit['message_hex']) == raw.split(b'\n\n', 1)[1]
        assert read('commit', rev=commit['oid'])['tree'] == commit['tree']
        checks['commits'] += 1
    for path in args.file or ['README.md']:
        expected = verify_content(read('file', path=path), args.revision, path)
        actual, headers = http(api + '/blob?' + urllib.parse.urlencode({'rev': args.revision, 'path': path}))
        assert actual == expected and headers['Content-Disposition'] == 'attachment', path
        checks['blobs'] += 1
    changes = read('changes')
    assert changes['base'] == git('rev-parse', args.revision + '^1').decode().strip()
    expected_paths = git('diff-tree', '--no-commit-id', '--no-renames', '--name-only', '-r', '-z', changes['base'], args.revision).split(b'\0')[:-1]
    assert [bytes.fromhex(change['path_hex']) for change in changes['changes']] == expected_paths
    for change in changes['changes']:
        diff = read('diff', path_hex=change['path_hex'])
        path = bytes.fromhex(change['path_hex']).decode()
        for side, revision in [('old', changes['base']), ('new', args.revision)]:
            if diff[side] is not None:
                verify_content(diff[side], revision, path)
        checks['diffs'] += 1
    # GitPath preserves dot components as raw names; it never traverses the filesystem.
    if args.blame:
        attribution = read('blame', path=args.blame)
        actual = [entry['commit']['oid'] for entry in attribution['ranges'] for _ in range(entry['lines'])]
        lines = git('blame', '--first-parent', '--line-porcelain', args.revision, '--', args.blame).splitlines()
        expected = [line.split()[0].decode() for line in lines if len(line.split()) in (3, 4) and len(line.split()[0]) == 40 and all(byte in b'0123456789abcdef' for byte in line.split()[0])]
        assert actual == expected, args.blame
        checks['blame_lines'] = len(actual)
    for suffix, status in [('tree?limit=0', 400), ('tree?path=../secret', 404), ('file?path=missing-file', 404), ('file?path=.', 404), ('tree?path_hex=ff0', 400), ('tree?cursor=bad', 400)]:
        body, _ = http(api + '/' + suffix, status)
        assert json.loads(body)['error']['message']
        checks['errors'] += 1
    http('/api/repos/unknown/missing/refs', 404)
    http('/api/repos', 403, headers={'Host': 'untrusted.invalid'})
    http('/assets/absent.js', 404)
    shell, headers = http('/' + args.repository)
    assert b'id="root"' in shell and "frame-ancestors 'none'" in headers['Content-Security-Policy']
    head, headers = http('/' + args.repository, method='HEAD')
    assert not head and int(headers['Content-Length']) == len(shell)
    print(json.dumps({'revision': args.revision, 'checks': checks, 'roundtrip_median_ms': {action: round(statistics.median(values), 3) for action, values in samples.items()}, 'timing_note': 'Mixed first/repeated requests; local process/transport/storage caches were not flushed.'}, indent=2))


if __name__ == '__main__':
    main()
