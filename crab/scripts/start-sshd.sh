#!/usr/bin/env bash
# Start a Docker container running an SSH server for local testing.
#
# Usage:
#   ./scripts/start-sshd.sh        # start the container
#   ./scripts/start-sshd.sh stop   # stop and remove the container
#
# Connection details:
#   Host:     localhost
#   Port:     2222
#   Username: root
#   Password: testpass

set -euo pipefail

CONTAINER_NAME="crab-ssh-test-host"
IMAGE="ubuntu:rolling"
SSH_PORT=2222
ROOT_PASSWORD="testpass"

if [[ "${1:-}" == "stop" ]]; then
  echo "Stopping $CONTAINER_NAME..."
  docker stop "$CONTAINER_NAME" 2>/dev/null || true
  docker rm "$CONTAINER_NAME" 2>/dev/null || true
  echo "Done."
  exit 0
fi

# Remove any existing container with the same name
docker rm -f "$CONTAINER_NAME" 2>/dev/null || true

echo "Starting SSH test container ($CONTAINER_NAME) on port $SSH_PORT..."

docker run -d \
  --name "$CONTAINER_NAME" \
  -p "${SSH_PORT}:22" \
  "$IMAGE" \
  bash -c "
    apt-get update -qq &&
    apt-get install -y -qq openssh-server > /dev/null 2>&1 &&
    mkdir -p /run/sshd &&
    echo 'root:${ROOT_PASSWORD}' | chpasswd &&
    sed -i 's/#PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config &&
    sed -i 's/#PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config &&
    /usr/sbin/sshd -D
  "

echo ""
echo "Waiting for SSH to be ready..."
for i in $(seq 1 30); do
  if docker exec "$CONTAINER_NAME" bash -c "ss -tlnp | grep -q :22" 2>/dev/null; then
    break
  fi
  sleep 1
done

echo ""
echo "SSH test container is ready!"
echo ""
echo "  Host:     localhost"
echo "  Port:     $SSH_PORT"
echo "  Username: root"
echo "  Password: $ROOT_PASSWORD"
echo ""
echo "Quick test:"
echo "  ssh -o StrictHostKeyChecking=no -p $SSH_PORT root@localhost"
echo ""
echo "Stop with:"
echo "  ./scripts/start-sshd.sh stop"
