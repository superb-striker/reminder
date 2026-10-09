# Deploying to the Oracle VM

Exact commands for each step. Run the VM-side commands over SSH; run the
last section from your laptop.

## 1. Create the VM

Oracle Cloud console → Compute → Instances → Create instance:
- Image: **Ubuntu 24.04**
- Shape: **Ampere A1 (ARM)**, Always Free eligible
- Save the generated SSH key pair, note the public IP

## 2. Lock down SSH and the firewall

```sh
ssh -i /path/to/key ubuntu@<vm-ip>

sudo sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config
sudo sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
sudo systemctl restart sshd

sudo ufw allow 22
sudo ufw allow 80
sudo ufw allow 443
sudo ufw enable

sudo apt update && sudo apt upgrade -y
sudo apt install -y unattended-upgrades
sudo dpkg-reconfigure -plow unattended-upgrades
```

The reminder server itself binds to `127.0.0.1` only (see
`REMINDER_BIND` in `reminder.env`), so it's never reachable from
outside the VM even without a firewall rule for it — only Caddy (80/443)
and SSH (22) need to be open.

## 3. Install build dependencies

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

sudo apt install -y build-essential pkg-config libssl-dev sqlite3
```

`libssl-dev` is needed because the HTTP client (`reqwest`) uses
`native-tls` (OpenSSL) rather than embedding its own TLS stack — see
the "Design decisions" section of the main README.

## 4. Build and install

```sh
git clone <this-repo> reminder-src
cd reminder-src
cargo build --release
# (On a modern rustup toolchain you can optionally `cargo update` first
# to drop this project's sandbox-only dependency pins -- see Cargo.toml.)

sudo useradd --system --no-create-home --shell /usr/sbin/nologin reminder
sudo mkdir -p /opt/reminder
sudo cp target/release/reminder /opt/reminder/
sudo cp -r migrations /opt/reminder/
sudo chown -R reminder:reminder /opt/reminder
```

## 5. Environment file and systemd service

```sh
sudo cp deploy/reminder.env.example /opt/reminder/reminder.env
sudo nano /opt/reminder/reminder.env     # fill in REMINDER_TOKEN, REMINDER_TZ, REMINDER_NTFY_TOPIC
# Generate a token with:
openssl rand -hex 32

sudo chmod 600 /opt/reminder/reminder.env
sudo chown reminder:reminder /opt/reminder/reminder.env

sudo cp deploy/reminder.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now reminder
sudo systemctl status reminder       # should show "active (running)"
sudo journalctl -u reminder -f       # watch logs
```

## 6. Caddy for HTTPS

Point your domain's A record at the VM's IP before this step — Caddy
needs it resolvable to issue a certificate.

```sh
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update && sudo apt install -y caddy

sudo cp deploy/Caddyfile /etc/caddy/Caddyfile
sudo nano /etc/caddy/Caddyfile       # replace reminders.yourdomain.com with your real domain
sudo systemctl restart caddy
```

## 7. Verify from your laptop

```sh
export REMINDER_SERVER_URL=https://reminders.yourdomain.com
export REMINDER_TOKEN=<the token from step 5>

reminder list
```

An empty "no reminders yet" response confirms the full path — laptop →
Caddy (TLS) → `reminder serve` (loopback) → SQLite — works end to end.

## 8. Backups

```sh
sudo cp deploy/backup.sh /opt/reminder/backup.sh
sudo chmod +x /opt/reminder/backup.sh
sudo chown reminder:reminder /opt/reminder/backup.sh

echo '0 3 * * * reminder /opt/reminder/backup.sh' | sudo tee /etc/cron.d/reminder-backup
```

## Updating later

```sh
cd reminder-src && git pull && cargo build --release
sudo systemctl stop reminder
sudo cp target/release/reminder /opt/reminder/
sudo systemctl start reminder
```
