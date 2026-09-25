#!/bin/sh
set -eu

umask 077
present=0
for file in /identity/ca.crt /identity/peer.crt /identity/peer.key; do
  if [ -e "$file" ]; then
    present=$((present + 1))
  fi
done
if [ "$present" -eq 3 ]; then
  exit 0
fi
if [ "$present" -ne 0 ]; then
  echo "Incomplete peer identity volume; use a fresh Compose project." >&2
  exit 1
fi

openssl genpkey -algorithm ED25519 -out /identity/ca.key
openssl req -x509 -new -days 7 -subj /CN=crab-compose-peer-ca \
  -addext basicConstraints=critical,CA:TRUE \
  -key /identity/ca.key -out /identity/ca.crt
openssl genpkey -algorithm ED25519 -out /identity/peer.key
openssl req -new -subj /CN=localhost \
  -key /identity/peer.key -out /identity/peer.csr
printf '%s\n' basicConstraints=critical,CA:FALSE \
  keyUsage=critical,digitalSignature \
  extendedKeyUsage=serverAuth,clientAuth \
  subjectAltName=DNS:localhost > /identity/peer.ext
openssl x509 -req -days 7 -CAcreateserial \
  -in /identity/peer.csr -CA /identity/ca.crt \
  -CAkey /identity/ca.key -extfile /identity/peer.ext \
  -out /identity/peer.crt
rm /identity/ca.key /identity/peer.csr /identity/peer.ext /identity/ca.srl
chmod 0444 /identity/ca.crt /identity/peer.crt
chmod 0400 /identity/peer.key
chown 10001:10001 /identity/ca.crt /identity/peer.crt /identity/peer.key
