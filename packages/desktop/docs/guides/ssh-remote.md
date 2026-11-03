# SSH Remote Connections Guide

## Quick Start

1. Open the SSH view from the Activity Bar (or `Cmd+Shift+S`)
2. Click "Add Connection" or "Import from SSH Config"
3. Enter your server details and test the connection
4. Click "Save & Connect"

## Setting Up a Connection

### From SSH Config

If you already have hosts in `~/.ssh/config`, click "Import from SSH Config"
to import them as connection profiles. You can selectively import specific
hosts.

### From a Template

Choose a pre-built template for your cloud provider:
- **AWS EC2**: Pre-configured for Ubuntu/Amazon Linux instances
- **GCP Compute**: Uses `~/.ssh/google_compute_engine` key
- **Lambda Labs**: GPU cloud with Ubuntu defaults
- **RunPod**: GPU pods with root access
- **Paperspace**: Gradient machines
- **Generic Linux**: Any SSH-accessible server

### Manual Configuration

Fill in the connection form:
- **Host**: Hostname or IP address
- **Port**: SSH port (default: 22)
- **Username**: Your user on the remote server
- **Authentication**: SSH Agent (recommended), Key File, or Password

## Authentication Methods

### SSH Agent (Recommended)

Uses your system's SSH agent (macOS Keychain, 1Password, ssh-agent).
No key file path needed — the agent handles key selection.

### Key File

Point to your private key file (e.g., `~/.ssh/id_ed25519`).
If the key has a passphrase, it's stored securely in your OS keychain.

### Password

Password is stored in your OS keychain (never in config files).
Less secure than key-based auth — use only when keys aren't available.

## Working with Remote Repos

Once connected, click "Browse Files" to explore the remote filesystem.
When you find a directory with a `.git` folder, click "Open as Repository"
to add it to your sidebar.

Remote repos appear with a coloured SSH badge and work identically to
local repos — git status, staging, committing, branching, and all other
operations work transparently.

## Port Forwarding

Forward remote ports to your local machine for accessing services like
Jupyter, TensorBoard, or web servers running on the remote.

### Manual Setup

1. Open the Port Forwards panel on a connected server
2. Click "Add Forward"
3. Enter local port, remote host (usually 127.0.0.1), and remote port
4. Click "Open in Browser" to access the forwarded service

### Auto-Detection

When you run a command in a remote terminal that starts a service,
Crab detects the port and offers to forward it automatically.

## System Health

Connected servers show system health in the connection card:
- Disk usage
- Memory usage
- GPU utilization and temperature (if NVIDIA GPUs are present)

Alerts appear when disk > 90%, memory > 90%, or GPU temp > 85°C.

## Troubleshooting

### Connection Refused

- Verify SSH server is running: `systemctl status sshd`
- Check the port is correct
- Verify firewall allows SSH traffic

### Authentication Failed

- Check username is correct
- Verify key permissions: `chmod 600 ~/.ssh/id_*`
- Try SSH agent: `ssh-add ~/.ssh/id_ed25519`
- Test from terminal: `ssh -v user@host`

### Agent Deployment Failed

- Check disk space: `df -h` (need ~20MB)
- Check permissions: `ls -la ~/.crab/bin/`
- Kill stale agent: `pkill crab-desktop-agent`

### High Latency

- Enable compression in profile settings
- The app automatically increases cache TTLs for slow connections
- Run diagnostics to see detailed latency and bandwidth info

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+S` | Open SSH View |
| `Cmd+Shift+T` | Open Remote Terminal (most recent connection) |
| `Cmd+R` | Refresh (invalidate cache) |

## FAQ

**Q: Does data transfer go through my local machine?**
A: No. When you push/pull on a remote repo, the data flows directly
between the remote server and cloud storage. Your local machine only
sends commands and receives status updates.

**Q: What happens if my connection drops?**
A: The app automatically reconnects with exponential backoff. Read-only
operations use cached data during reconnection. Write operations wait
for the connection to restore.

**Q: Can I use jump hosts / bastion servers?**
A: Yes. Set the "Jump Host" field in the connection form. Multi-hop
chains are supported.

**Q: Is it safe to use on production servers?**
A: The remote agent runs as your user with no privilege escalation.
It only accesses files your user can access. All connections are
encrypted via SSH.
