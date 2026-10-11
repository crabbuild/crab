use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use crab_storage::{RetryPolicy, Store};
use object_store::RetryConfig;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;

#[tokio::test]
#[ignore = "allocates and sends 2 GiB over a real local HTTP connection"]
async fn large_conditional_upload_preserves_bytes_in_one_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || -> std::io::Result<_> {
        let (stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let request = line.trim().to_owned();
        let mut length = 0;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut remaining = length;
        let mut hash = blake3::Hasher::new();
        let mut buffer = [0; 64 * 1024];
        while remaining > 0 {
            let count = remaining.min(buffer.len());
            reader.read_exact(&mut buffer[..count])?;
            hash.update(&buffer[..count]);
            remaining -= count;
        }
        reader.get_mut().write_all(
            b"HTTP/1.1 200 OK\r\nETag: \"uploaded\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        Ok((request, length, hash.finalize()))
    });
    let inner = AmazonS3Builder::new()
        .with_bucket_name("bucket")
        .with_access_key_id("test")
        .with_secret_access_key("test")
        .with_region("us-east-1")
        .with_endpoint(endpoint)
        .with_allow_http(true)
        .with_retry(RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        })
        .build()
        .unwrap();
    let store = Store::with_retry(
        Arc::new(inner),
        RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::DEFAULT
        },
    );
    let mut body = vec![0; 2 * 1024 * 1024 * 1024];
    body[0] = 1;
    *body.last_mut().unwrap() = 2;
    let expected = (
        "PUT /bucket/objects/large HTTP/1.1".to_owned(),
        body.len(),
        blake3::hash(&body),
    );
    let uploaded = store
        .put(&Path::from("objects/large"), Bytes::from(body))
        .await;
    let received = server.join().unwrap();
    uploaded.unwrap();
    assert_eq!(received.unwrap(), expected);
}
