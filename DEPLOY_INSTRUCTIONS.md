# GEM_RUST Sweden: деплой на GCP VPS

Цель: поднять отдельный `GEM_RUST` VPS в Швеции и запускать один BTC 5m Strategy J/endgame через встроенный `--server`.

Все важные ресурсы специально содержат `sweden`, чтобы их нельзя было спутать с другими странами.

```text
GCP VM:          gem-rust-sweden-vps
Region:          europe-north2
Zone:            europe-north2-b
Machine:         e2-medium
Disk:            30GB pd-ssd
OS:              Debian 12
Static IP:       gem-rust-sweden-ip
Firewall:        gem-rust-sweden-8787
Network tag:     gem-rust-sweden
Repo dir:        ~/bots/gem-rust-sweden-btc-5m
Repo URL:        https://github.com/boriskaborisenko/GEM_RUST.git
Binary:          target/release/gem_rust
Bot bind:        0.0.0.0:8787
Bot API:         http://$SWEDEN_VPS_IP:8787
Secrets:         .env.live
Systemd service: gem-rust-sweden-btc-5m.service
```

На Sweden VPS открывается один внешний порт:

```text
http://$SWEDEN_VPS_IP:8787
```

---

## 1. Mac: подготовить GCP под Sweden

```bash
gcloud auth login
gcloud config set project tidal-vim-490321-j2
gcloud config set compute/region europe-north2
gcloud config set compute/zone europe-north2-b
gcloud config list
gcloud services enable compute.googleapis.com
gcloud services list --enabled --filter="compute.googleapis.com"
```

---

## 2. Mac: создать Sweden static IP

```bash
gcloud compute addresses create gem-rust-sweden-ip \
  --region=europe-north2
```

Показать Sweden IP:

```bash
SWEDEN_VPS_IP=$(gcloud compute addresses describe gem-rust-sweden-ip \
  --region=europe-north2 \
  --format='get(address)')

echo "$SWEDEN_VPS_IP"
gcloud compute addresses list
```

---

## 3. Mac: создать Sweden VPS

```bash
gcloud compute instances create gem-rust-sweden-vps \
  --zone=europe-north2-b \
  --machine-type=e2-medium \
  --image-family=debian-12 \
  --image-project=debian-cloud \
  --boot-disk-size=30GB \
  --boot-disk-type=pd-ssd \
  --address=gem-rust-sweden-ip \
  --tags=gem-rust-sweden
```

Открыть порт `8787` для Sweden bot API:

```bash
gcloud compute firewall-rules create gem-rust-sweden-8787 \
  --allow=tcp:8787 \
  --target-tags=gem-rust-sweden \
  --direction=INGRESS \
  --priority=1000
```

Проверить:

```bash
gcloud compute instances list
gcloud compute firewall-rules list --filter="name:gem-rust-sweden"
```

---

## 4. Mac: подключиться к Sweden VPS

```bash
gcloud compute ssh gem-rust-sweden-vps --zone=europe-north2-b
```

Дальше команды выполняются внутри Sweden VPS.

---

## 5. Sweden VPS: установить системные пакеты

```bash
sudo apt update
sudo apt upgrade -y
sudo apt install -y git curl build-essential pkg-config libssl-dev ca-certificates nano htop sqlite3
```

---

## 6. Sweden VPS: установить Rust

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

## 7. Sweden VPS: клонировать GEM_RUST

```bash
mkdir -p ~/bots
cd ~/bots
git clone https://github.com/boriskaborisenko/GEM_RUST.git gem-rust-sweden-btc-5m
cd gem-rust-sweden-btc-5m
```

---

## 8. Sweden VPS: настроить `.env.live`

```bash
cd ~/bots/gem-rust-sweden-btc-5m
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

## 9. Sweden VPS: проверить `config.json`

```bash
cd ~/bots/gem-rust-sweden-btc-5m
grep -nE '"strategy"|execution|"secretsFile"|"mode"|"dryRun"' config.json
```

Минимально нужные значения:

```text
"strategy": "j_endgame"
"secretsFile": ".env.live"
```

Для настоящего live проверь, что включен live execution и нет dry-run:

```text
"mode": "live"
"dryRun": false
```

Если запускаешь через CLI, `--live` должен быть без `--dry-run`.

---

## 10. Sweden VPS: собрать release

```bash
cd ~/bots/gem-rust-sweden-btc-5m
cargo build --release
ls -lh target/release/gem_rust
```

Важно: если запускаешь `target/release/gem_rust`, то после `git pull` или любых изменений кода нужно снова делать:

```bash
cargo build --release
```

Иначе будет запускаться старый release binary.

---

## 11. Sweden VPS: запустить paper server

```bash
cd ~/bots/gem-rust-sweden-btc-5m
./target/release/gem_rust --server --stop
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

## 12. Sweden VPS: запустить live server

Сначала остановить paper/server, если он уже запущен:

```bash
cd ~/bots/gem-rust-sweden-btc-5m
./target/release/gem_rust --server --stop
```

Запуск live:

```bash
./target/release/gem_rust BTC 5m --live --server --server-bind 0.0.0.0:8787
```

Смотреть live лог:

```bash
tail -f logs/gem_rust_server.log
```

Проверить последние live order events:

```bash
sqlite3 "$(ls -td logs/runs/* | head -1)/live_audit.sqlite3" \
  ".mode line" \
  "select id, window_number, operation, side, amount, executed, reject_reason from order_events order by id desc limit 10;"
```

---

## 13. Mac: проверить внешний Sweden bot API

```bash
SWEDEN_VPS_IP=$(gcloud compute addresses describe gem-rust-sweden-ip \
  --region=europe-north2 \
  --format='get(address)')

curl http://$SWEDEN_VPS_IP:8787/api/health
curl http://$SWEDEN_VPS_IP:8787/api/state
```

---

## 14. Sweden VPS: обновление после нового commit

```bash
cd ~/bots/gem-rust-sweden-btc-5m
./target/release/gem_rust --server --stop
git pull
cargo build --release
```

Потом снова запустить paper:

```bash
./target/release/gem_rust BTC 5m --paper --server --server-bind 0.0.0.0:8787
```

Или live:

```bash
./target/release/gem_rust BTC 5m --live --server --server-bind 0.0.0.0:8787
```

---

## 15. Если `git pull` ругается на tracked logs

Если видишь такое:

```text
error: Your local changes to the following files would be overwritten by merge:
        logs/gem_rust_server.log
```

Если лог нужен, сначала сохранить копию:

```bash
cd ~/bots/gem-rust-sweden-btc-5m
mkdir -p /tmp/gem-rust-sweden-log-backup
cp logs/gem_rust_server.log /tmp/gem-rust-sweden-log-backup/gem_rust_server.log
git checkout -- logs/gem_rust_server.log
git pull
cp /tmp/gem-rust-sweden-log-backup/gem_rust_server.log logs/gem_rust_server.log
```

Если лог не нужен:

```bash
cd ~/bots/gem-rust-sweden-btc-5m
git checkout -- logs/gem_rust_server.log
git pull
```

После `git pull` не забыть:

```bash
cargo build --release
```

---

## 16. Sweden VPS: systemd service

Создать service:

```bash
sudo nano /etc/systemd/system/gem-rust-sweden-btc-5m.service
```

Содержимое:

```ini
[Unit]
Description=GEM_RUST Sweden BTC 5m
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=boris
WorkingDirectory=/home/boris/bots/gem-rust-sweden-btc-5m
Environment=RUST_BACKTRACE=1
ExecStart=/home/boris/bots/gem-rust-sweden-btc-5m/target/release/gem_rust BTC 5m --live --server --server-bind 0.0.0.0:8787
Restart=always
RestartSec=5
PIDFile=/home/boris/bots/gem-rust-sweden-btc-5m/logs/gem_rust_server.pid

[Install]
WantedBy=multi-user.target
```

Включить и запустить:

```bash
sudo systemctl daemon-reload
sudo systemctl enable gem-rust-sweden-btc-5m
sudo systemctl start gem-rust-sweden-btc-5m
sudo systemctl status gem-rust-sweden-btc-5m
```

Логи systemd:

```bash
journalctl -u gem-rust-sweden-btc-5m -f
```

Остановить:

```bash
sudo systemctl stop gem-rust-sweden-btc-5m
```

---

## 17. Быстрый Sweden checklist

```text
[ ] GCP region/zone: europe-north2 / europe-north2-b
[ ] Static IP: gem-rust-sweden-ip
[ ] VM: gem-rust-sweden-vps
[ ] Firewall: gem-rust-sweden-8787
[ ] Repo dir: ~/bots/gem-rust-sweden-btc-5m
[ ] .env.live заполнен и chmod 600
[ ] config.json смотрит на .env.live
[ ] cargo build --release выполнен после последнего git pull
[ ] paper server проверен
[ ] live server запускается только после проверки paper
[ ] http://$SWEDEN_VPS_IP:8787/api/health отвечает
```
