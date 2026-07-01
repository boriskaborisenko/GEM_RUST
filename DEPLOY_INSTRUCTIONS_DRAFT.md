# GEM_RUST: деплой на GCP VPS

Цель: поднять `GEM_RUST` на GCP VPS и запускать один BTC 5m Strategy J через встроенный `--server`.

Фиксированный путь деплоя:

```text
GCP VM:      gem-rust-vps
Region:      europe-west1
Zone:        europe-west1-b
Machine:     e2-medium
Disk:        30GB pd-ssd
OS:          Debian 12
Repo:        https://github.com/boriskaborisenko/GEM_RUST.git
Binary:      target/release/gem_rust
Bot bind:    0.0.0.0:8787
Bot API:     http://$VPS_IP:8787
Secrets:     .env.live
```

На bot-VPS открывается один внешний порт:

```text
http://$VPS_IP:8787
```

---

## 1. Mac: подготовить GCP

```bash
gcloud auth login
gcloud config set project tidal-vim-490321-j2
gcloud config set compute/region europe-west1
gcloud config set compute/zone europe-west1-b
gcloud config list
gcloud services enable compute.googleapis.com
gcloud services list --enabled --filter="compute.googleapis.com"
```

---

## 2. Mac: создать статический IP

```bash
gcloud compute addresses create gem-rust-ireland-ip \
  --region=europe-west1
```

Показать IP:

```bash
VPS_IP=$(gcloud compute addresses describe gem-rust-ireland-ip \
  --region=europe-west1 \
  --format='get(address)')

echo "$VPS_IP"
gcloud compute addresses list
```

---

## 3. Mac: создать VPS

```bash
gcloud compute instances create gem-rust-vps \
  --zone=europe-west1-b \
  --machine-type=e2-medium \
  --image-family=debian-12 \
  --image-project=debian-cloud \
  --boot-disk-size=30GB \
  --boot-disk-type=pd-ssd \
  --address=gem-rust-ireland-ip \
  --tags=gem-rust
```

Открыть порт `8787` для bot API:

```bash
gcloud compute firewall-rules create gem-rust-8787 \
  --allow=tcp:8787 \
  --target-tags=gem-rust \
  --direction=INGRESS \
  --priority=1000
```

Проверить:

```bash
gcloud compute instances list
```

---

## 4. Mac: подключиться к VPS

```bash
gcloud compute ssh gem-rust-vps --zone=europe-west1-b
```

Дальше команды выполняются внутри VPS.

---

## 5. VPS: установить системные пакеты

```bash
sudo apt update
sudo apt upgrade -y
sudo apt install -y git curl build-essential pkg-config libssl-dev ca-certificates nano htop
```

---

## 6. VPS: установить Rust

```bash
curl https://sh.rustup.rs -sSf | sh -s -- -y
source "$HOME/.cargo/env"
```

Проверить:

```bash
rustc --version
cargo --version
```

---

## 7. VPS: клонировать BTC 5m

```bash
mkdir -p ~/bots
cd ~/bots
git clone https://github.com/boriskaborisenko/GEM_RUST.git gem-btc-5m
cd gem-btc-5m
```

---

## 8. VPS: настроить `.env.live`

```bash
cd ~/bots/gem-btc-5m
cp .env.live.example .env.live
nano .env.live
chmod 600 .env.live
```

Заполнить:

```text
POLYMARKET_PRIVATE_KEY=
POLYMARKET_DEPOSIT_WALLET_ADDRESS=
CLOB_API_URL=https://clob.polymarket.com
POLY_RELAYER_API_KEY=
POLY_RELAYER_ADDRESS=
```

---

## 9. VPS: проверить `config.json`

```bash
grep -nE '"strategy"|execution|"secretsFile"' config.json
```

Нужные значения:

```text
"strategy": "j_endgame"
"secretsFile": ".env.live"
```

---

## 10. VPS: собрать release

```bash
cd ~/bots/gem-btc-5m
cargo build --release
ls -lh target/release/gem_rust
```

---

## 11. VPS: запустить paper server

```bash
cd ~/bots/gem-btc-5m
./target/release/gem_rust BTC 5m --paper --server --server-bind 0.0.0.0:8787
```

Проверить статус:

```bash
./target/release/gem_rust --server --status
```

Смотреть лог:

```bash
tail -f logs/gem_rust_server.log
```

Остановить:

```bash
./target/release/gem_rust --server --stop
```

---

## 12. Mac: проверить внешний bot API

Проверить API:

```bash
VPS_IP=$(gcloud compute addresses describe gem-rust-ireland-ip \
  --region=europe-west1 \
  --format='get(address)')

curl http://$VPS_IP:8787/api/health
curl http://$VPS_IP:8787/api/state
```

---

## 13.0 VPS: запустить paper

```bash
cd ~/bots/gem-btc-5m
./target/release/gem_rust --server --stop
./target/release/gem_rust BTC 5m --paper  --server --server-bind 0.0.0.0:8787
```

## 13. VPS: запустить live dry-run

```bash
cd ~/bots/gem-btc-5m
./target/release/gem_rust --server --stop
./target/release/gem_rust BTC 5m --live --dry-run --server --server-bind 0.0.0.0:8787
```

Проверить:

```bash
./target/release/gem_rust --server --status
tail -f logs/gem_rust_server.log
```

---

## 14. VPS: запустить real live

```bash
cd ~/bots/gem-btc-5m
./target/release/gem_rust --server --stop
./target/release/gem_rust BTC 5m --live --server --server-bind 0.0.0.0:8787
```

Проверить:

```bash
./target/release/gem_rust --server --status
tail -f logs/gem_rust_server.log
```

---

## 15. VPS: команды управления BTC 5m

```bash
cd ~/bots/gem-btc-5m
./target/release/gem_rust --server --status
./target/release/gem_rust --server --stop
./target/release/gem_rust --server --stop --force
tail -f logs/gem_rust_server.log
```

---

## 16. VPS: обновить BTC 5m

```bash
cd ~/bots/gem-btc-5m
./target/release/gem_rust --server --stop
git pull
cargo build --release
./target/release/gem_rust BTC 5m --live --server --server-bind 0.0.0.0:8787
```

---

## 17. VPS: systemd для BTC 5m

```bash
sudo nano /etc/systemd/system/gem-btc-5m.service
```

Вставить:

```ini
[Unit]
Description=GEM_RUST BTC 5m
After=network-online.target
Wants=network-online.target

[Service]
Type=forking
User=boris
WorkingDirectory=/home/boris/bots/gem-btc-5m
ExecStart=/home/boris/bots/gem-btc-5m/target/release/gem_rust BTC 5m --live --server --server-bind 0.0.0.0:8787
ExecStop=/home/boris/bots/gem-btc-5m/target/release/gem_rust --server --stop
PIDFile=/home/boris/bots/gem-btc-5m/logs/gem_rust_server.pid
Restart=on-failure
RestartSec=10
TimeoutStartSec=120
TimeoutStopSec=40

[Install]
WantedBy=multi-user.target
```

Команды:

```bash
sudo systemctl daemon-reload
sudo systemctl enable gem-btc-5m
sudo systemctl start gem-btc-5m
sudo systemctl status gem-btc-5m
journalctl -u gem-btc-5m -f
```
