"""Temporary dependency qualification using the actual Crab build artifacts."""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import ssl
import subprocess
import sys
import tempfile
import threading

parser = argparse.ArgumentParser()
parser.add_argument('--artifacts', type=Path, required=True)
parser.add_argument('--root', type=Path, required=True)
parser.add_argument('--toolchain')
args = parser.parse_args()
root = args.root
root.mkdir(parents=True, exist_ok=False)
fixture = Path(tempfile.mkdtemp(prefix='fixture-', dir=root))
commands = [
    ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-subj', '/CN=Crab qualification CA', '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign,cRLSign', '-keyout', 'ca.key', '-out', 'ca.pem'],
    ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-subj', '/CN=Crab other qualification CA', '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign,cRLSign', '-keyout', 'other.key', '-out', 'other.pem'],
    ['req', '-newkey', 'rsa:2048', '-nodes', '-subj', '/CN=localhost', '-keyout', 'server.key', '-out', 'server.csr'],
    ['x509', '-req', '-in', 'server.csr', '-CA', 'ca.pem', '-CAkey', 'ca.key', '-CAcreateserial', '-days', '1', '-extfile', 'server.ext', '-out', 'server.pem'],
]
(fixture/'server.ext').write_text('subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n')
with (root/'openssl.log').open('wb') as log:
    for command in commands:
        subprocess.run(['openssl', *command], cwd=fixture, stdout=log, stderr=subprocess.STDOUT, check=True)
artifacts = [json.loads(line) for line in args.artifacts.read_text().splitlines() if line.startswith('{')]
source = Path(__file__).with_name('native_cert_contract.rs')
command = ['rustc', *(['+'+args.toolchain] if args.toolchain else []), '--edition=2024', '--test', str(source), '-o', str(root/'contract')]
linked = {}
for name, version in [('object_store','0.14.1'), ('tokio','1.53.1'), ('rustls_native_certs','0.8.5'), ('rustls_pki_types','1.14.1')]:
    selected = [a for a in artifacts if a.get('reason')=='compiler-artifact' and a.get('target',{}).get('name')==name and a['package_id'].endswith(('@'+version, '#'+version))]
    assert len(selected)==1, (name,len(selected))
    if name == 'rustls_pki_types':
        assert selected[0]['package_id'].startswith('path+'), selected[0]['package_id']
    library = next(p for p in selected[0]['filenames'] if p.endswith('.rlib'))
    command += ['--extern', f'{name}={library}', '-L', 'dependency='+str(Path(library).parent)]
    linked[name] = {'version': version, 'rlib_sha256': hashlib.sha256(Path(library).read_bytes()).hexdigest()}
for path in sorted({p for a in artifacts for p in a.get('linked_paths', [])}):
    command += ['-L', path]
subprocess.run(command, check=True)
requests = []
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        requests.append(self.path)
        self.send_response(200)
        for key,value in [('Content-Length','9'),('Last-Modified','Mon, 01 Jan 2024 00:00:00 GMT'),('ETag','"qualification"')]:
            self.send_header(key,value)
        self.end_headers()
        self.wfile.write(b'tls-probe')
    def log_message(self,*args):
        pass
server = http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(fixture/'server.pem', fixture/'server.key')
server.socket = context.wrap_socket(server.socket, server_side=True)
worker = threading.Thread(target=server.serve_forever, daemon=True)
worker.start()
(fixture/'native.pem').write_bytes((fixture/'ca.pem').read_bytes())
(fixture/'empty').mkdir()
env = {k:v for k,v in os.environ.items() if not k.startswith(('SSL_CERT_', 'CERT_'))}
env |= {'CERT_CA':str(fixture/'ca.pem'), 'CERT_OTHER_CA':str(fixture/'other.pem'), 'CERT_FIXTURE':str(fixture), 'CERT_ENDPOINT':f'https://localhost:{server.server_port}', 'SSL_CERT_FILE':str(fixture/'native.pem'), 'SSL_CERT_DIR':str(fixture/'empty')}
tests = ['explicit_tls_trust_and_constructor_failure','empty_directory_environment_entries_are_ignored']
if os.name != 'nt':
    tests.append('file_identity_preserves_distinct_roots_and_errors')
if sys.platform=='linux':
    tests.append('native_tls_reloads_roots_and_rejects_invalid_trust')
try:
    for test in tests:
        case_env = env | ({'SSL_CERT_FILE':str(fixture/'ca.pem'), 'SSL_CERT_DIR':os.pathsep} if test.startswith('empty_directory') else {})
        with (root/(test+'.log')).open('wb') as log:
            subprocess.run([str(root/'contract'),'--exact',test,'--nocapture'],env=case_env,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=60)
        assert '1 passed; 0 failed' in (root/(test+'.log')).read_text(), test
    assert requests == ['/qualification/probe']*(3 if sys.platform=='linux' else 1), requests
    proof = {'status':'pass','platform':sys.platform,'tests':tests,'linked':linked,'source_sha256':hashlib.sha256(source.read_bytes()).hexdigest(),'successful_http_requests':len(requests)}
    (root/'verified-tls-contract.json').write_text(json.dumps(proof,indent=2)+'\n')
    print(json.dumps(proof))
finally:
    server.shutdown()
    server.server_close()
    worker.join()
