# Deployment Guide

This guide covers production deployment considerations for HIVE in a multi-machine environment.

## Security Model

HIVE's security relies on multiple layers:

1. **Network Boundary**: Bind services to specific interfaces, not `0.0.0.0`
2. **Authentication**: Password protection for web interfaces
3. **Process Isolation**: Supervised execution with watchdog monitoring

### Web Interface Security

The `hive-web` service exposes terminal functionality and should be deployed carefully:

- **Bind to loopback or private network** by default (`127.0.0.1:8080`)
- **Use strong passwords** via `HIVE_WEB_PASSWORD` environment variable (minimum 8 characters)
- **Never bind to public interfaces** without additional security layers
- **Tokens are in-memory** - restarts log all users out

### Environment Variables

Secure configuration using environment variables:

```bash
# Web interface
export HIVE_WEB_PASSWORD="your_secure_password"
export HIVE_WEB_ADDR="127.0.0.1:8080"

# Configuration paths
export HIVE_CONFIG_ROOT="/path/to/config"
export HIVE_WEB_STATIC="/path/to/static/files"
export HIVE_MASTER_NAME="your-master-name"

# Logging
export RUST_LOG="hive_web=info,hive_core=info"
```

## System Service Configuration

### systemd Service Unit

Example systemd service file (`/etc/systemd/system/hive-web.service`):

```ini
[Unit]
Description=HIVE Web Interface
After=network.target

[Service]
Type=simple
User=hive
Group=hive
WorkingDirectory=/home/hive/hive
EnvironmentFile=/home/hive/.config/hive/web.env
ExecStart=/home/hive/hive/target/release/hive-web
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

### Environment File

Create `/home/hive/.config/hive/web.env` with proper permissions:

```bash
chmod 600 /home/hive/.config/hive/web.env
```

Content:
```
HIVE_WEB_PASSWORD=your_secure_password
HIVE_WEB_ADDR=127.0.0.1:8080
HIVE_CONFIG_ROOT=/home/hive/hive
HIVE_WEB_STATIC=/home/hive/hive/hive-web/static
HIVE_MASTER_NAME=hive-master
RUST_LOG=hive_web=info,hive_core=info
```

### Launch Script

For systems using alternative init systems, create a launch script that sources the environment properly:

```bash
#!/bin/bash
set -a
. "$HOME/.config/hive/web.env"
set +a

export HIVE_WEB_ADDR="${HIVE_WEB_ADDR:-127.0.0.1:8080}"
export HIVE_CONFIG_ROOT="${HIVE_CONFIG_ROOT:-$HOME/hive}"
export HIVE_WEB_STATIC="${HIVE_WEB_STATIC:-$HOME/hive/hive-web/static}"
export HIVE_MASTER_NAME="${HIVE_MASTER_NAME:-$(hostname)}"
export RUST_LOG="${RUST_LOG:-hive_web=info,hive_core=info}"
export PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"

exec "$HOME/hive/target/release/hive-web"
```

## Database and Storage

- **Database location**: Configure `database.path` in `hive.toml`
- **Permissions**: Ensure the running user has read/write access
- **Backup strategy**: Regular backups of the SQLite database
- **Retention**: Consider log rotation and archive policies

## Worker Configuration

Configure `config/workers.toml` with your fleet:

```toml
# Add one [[workers]] block per machine
[[workers]]
name = "worker-1"
host = "worker1.example.com"
user = "hive-worker"
tags = ["gpu", "powerful"]

[[workers]]
name = "worker-2"
host = "worker2.example.com"
user = "hive-worker"
tags = ["cpu", "general"]
```

Ensure SSH keys are properly configured and host keys are trusted.

## Monitoring and Maintenance

- **Health checks**: The `/api/health` endpoint returns service status
- **Log management**: Use structured logging and log rotation
- **Process supervision**: Use systemd, launchd, or similar for restarts
- **Resource limits**: Consider ulimits and containerization for isolation

## Updating

1. Stop the service
2. Pull the latest code
3. Run `cargo build --release`
4. Restart the service

```bash
systemctl stop hive-web
git pull
cargo build --release
systemctl start hive-web
```

## Troubleshooting

- Check logs: `journalctl -u hive-web -f` (systemd) or `tail -f` on log files
- Verify environment: `echo $HIVE_WEB_PASSWORD` (should be set)
- Test connectivity: Ensure SSH keys work for configured workers
- Monitor resources: Check memory and CPU usage during operation