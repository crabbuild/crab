import os
from pathlib import Path
import subprocess

root = Path(os.environ['QUAL_ROOT'])
meter = root / 'metrics'
meter.mkdir()
for name in ['client_temp', 'proxy_temp']:
    (meter / name).mkdir()
conf = r'''
pid __ROOT__/metrics/nginx.pid;
error_log __ROOT__/metrics/error.log warn;
worker_processes auto;
worker_rlimit_nofile 65536;
events { worker_connections 4096; }
http {
    client_body_temp_path __ROOT__/metrics/client_temp;
    proxy_temp_path __ROOT__/metrics/proxy_temp;
    map "$arg_uploadid:$arg_partnumber" $upload_query {
        "~^.+:.+$" "uploadId=present&partNumber=1";
        "~^.+:$" "uploadId=present";
        default "";
    }
    map $args $safe_query {
        "~*(^|&)list-type=" "list-type=2";
        "~*(^|&)versions(=|&|$)" "versions";
        "~*(^|&)uploads(=|&|$)" "uploads";
        "~*(^|&)delete(=|&|$)" "delete";
        default $upload_query;
    }
    log_format s3 escape=json '{"method":"$request_method","path":"$uri","query":"$safe_query","prefix":"$arg_prefix","range":"$http_range","copy":"$http_x_amz_copy_source","status":$status,"upstream_status":"$upstream_status","request_length":$request_length,"content_length":"$http_content_length","transfer_encoding":"$http_transfer_encoding","body_bytes":$body_bytes_sent,"elapsed":"$request_time"}';
    access_log __ROOT__/metrics/access.jsonl s3;
    upstream rustfs_origin { server 127.0.0.1:9000; keepalive 64; }
    server {
        listen 127.0.0.1:19020;
        client_max_body_size 0;
        gzip off;
        location / {
            proxy_pass http://rustfs_origin;
            proxy_http_version 1.1;
            proxy_set_header Host $http_host;
            proxy_set_header Connection "";
            proxy_cache off;
            proxy_buffering off;
            proxy_request_buffering off;
            proxy_next_upstream off;
        }
    }
}
'''.replace('__ROOT__', str(root))
path = meter / 'nginx.conf'
path.write_text(conf)
subprocess.run(['nginx', '-p', str(meter), '-c', str(path), '-t'], check=True)
subprocess.run(['nginx', '-p', str(meter), '-c', str(path)], check=True)

