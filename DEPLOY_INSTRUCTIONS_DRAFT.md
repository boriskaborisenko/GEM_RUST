# GEM_RUST: деплой на GCP VPS

Цель: поднять `GEM_RUST` на GCP VPS и запускать Strategy J через встроенный `--server`.

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
Dashboard:   127.0.0.1:8787
Access:      SSH tunnel
Secrets:     .env.live
```

Dashboard открывается только через SSH tunnel:

```text
Mac -> SSH tunnel -> VPS 127.0.0.1:8787
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
gcloud compute addresses describe gem-rust-ireland-ip \
  --region=europe-west1 \
  --format='get(address)'

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

## 7. VPS: клонировать первый инстанс BTC 5m

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
./target/release/gem_rust BTC 5m --paper --server --server-bind 127.0.0.1:8787
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

## 12. Mac: открыть dashboard через SSH tunnel

Открыть отдельный терминал на Mac:

```bash
gcloud compute ssh gem-rust-vps --zone=europe-west1-b -- \
  -L 8787:127.0.0.1:8787
```

Открыть в браузере на Mac:

```text
http://127.0.0.1:8787
```

Проверить API:

```bash
curl http://127.0.0.1:8787/api/health
curl http://127.0.0.1:8787/api/state
```

---

## 13. VPS: запустить live dry-run

```bash
cd ~/bots/gem-btc-5m
./target/release/gem_rust --server --stop
./target/release/gem_rust BTC 5m --live --dry-run --server --server-bind 127.0.0.1:8787
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
./target/release/gem_rust BTC 5m --live --server --server-bind 127.0.0.1:8787
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

## 16. VPS: второй инстанс ETH 5m

Каждый инстанс живет в отдельной папке.

```bash
cd ~/bots
git clone https://github.com/boriskaborisenko/GEM_RUST.git gem-eth-5m
cd gem-eth-5m
cp .env.live.example .env.live
nano .env.live
chmod 600 .env.live
cargo build --release
```

Для ETH использовать отдельный Polymarket account/deposit wallet. Так каждый процесс сайзится от своего банка.

Запуск ETH:

```bash
cd ~/bots/gem-eth-5m
./target/release/gem_rust ETH 5m --live --server --server-bind 127.0.0.1:8788
```

Статус ETH:

```bash
cd ~/bots/gem-eth-5m
./target/release/gem_rust --server --status
```

---

## 17. Mac: tunnel для BTC + ETH

```bash
gcloud compute ssh gem-rust-vps --zone=europe-west1-b -- \
  -L 8787:127.0.0.1:8787 \
  -L 8788:127.0.0.1:8788
```

Открыть:

```text
BTC: http://127.0.0.1:8787
ETH: http://127.0.0.1:8788
```

---

## 18. VPS: обновить BTC 5m

```bash
cd ~/bots/gem-btc-5m
./target/release/gem_rust --server --stop
git pull
cargo build --release
./target/release/gem_rust BTC 5m --live --server --server-bind 127.0.0.1:8787
```

---

## 19. VPS: обновить ETH 5m

```bash
cd ~/bots/gem-eth-5m
./target/release/gem_rust --server --stop
git pull
cargo build --release
./target/release/gem_rust ETH 5m --live --server --server-bind 127.0.0.1:8788
```

---

## 20. VPS: systemd для BTC 5m

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
ExecStart=/home/boris/bots/gem-btc-5m/target/release/gem_rust BTC 5m --live --server --server-bind 127.0.0.1:8787
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

---

## 21. VPS: systemd для ETH 5m

```bash
sudo nano /etc/systemd/system/gem-eth-5m.service
```

Вставить:

```ini
[Unit]
Description=GEM_RUST ETH 5m
After=network-online.target
Wants=network-online.target

[Service]
Type=forking
User=boris
WorkingDirectory=/home/boris/bots/gem-eth-5m
ExecStart=/home/boris/bots/gem-eth-5m/target/release/gem_rust ETH 5m --live --server --server-bind 127.0.0.1:8788
ExecStop=/home/boris/bots/gem-eth-5m/target/release/gem_rust --server --stop
PIDFile=/home/boris/bots/gem-eth-5m/logs/gem_rust_server.pid
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
sudo systemctl enable gem-eth-5m
sudo systemctl start gem-eth-5m
sudo systemctl status gem-eth-5m
journalctl -u gem-eth-5m -f
```

---

## 22. Финальный live-чеклист

```text
[ ] VPS создан: gem-rust-vps
[ ] Rust установлен
[ ] Репозиторий склонирован в ~/bots/gem-btc-5m
[ ] .env.live заполнен
[ ] chmod 600 .env.live выполнен
[ ] cargo build --release выполнен
[ ] paper server запускался
[ ] SSH tunnel открывает dashboard
[ ] live dry-run показывает CLOB AUTH ok
[ ] real live запускается командой из раздела 14
```
