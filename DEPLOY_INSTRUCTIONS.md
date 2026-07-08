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

---

## 18. Caddy HTTPS reverse proxy для домена

Использовать Caddy как HTTPS-прокси перед Rust-сервером:

```text
https://api.your-domain.com:443
    -> Caddy
    -> http://127.0.0.1:8787 или http://0.0.0.0:8787
    -> GEM_RUST
```

Текущий рабочий вариант без остановки/перенастройки Rust:

```text
GEM_RUST остается на 0.0.0.0:8787
Caddy добавляется сверху для https://api-domain
Старый доступ http://SWEDEN_VPS_IP:8787 остается рабочим
```

### 18.1 DNS и firewall

В DNS домена создать `A` record:

```text
api.your-domain.com -> SWEDEN_VPS_IP
```

Если на сервере установлен `ufw`, открыть на VPS HTTP/HTTPS:

```bash
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw status
```

Если `ufw: command not found`, ничего страшного. Для GCP главное открыть Google Cloud firewall.

Для GCP открыть `tcp:80,443` на VM tag `gem-rust-sweden`:

```bash
gcloud compute firewall-rules create gem-rust-sweden-http-https \
  --direction=INGRESS \
  --priority=1000 \
  --network=default \
  --action=ALLOW \
  --rules=tcp:80,tcp:443 \
  --source-ranges=0.0.0.0/0 \
  --target-tags=gem-rust-sweden
```

Проверить GCP firewall rules:

```bash
gcloud compute firewall-rules list --filter="name:gem-rust-sweden"
```

Важно: старый внешний `8787` не трогать. `gem-rust-sweden-8787` остается как есть, Rust продолжает работать на `0.0.0.0:8787`, Caddy просто добавляется сверху для HTTPS-домена.

### 18.2 Установка Caddy на Ubuntu/Debian

```bash
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg
sudo chmod o+r /etc/apt/sources.list.d/caddy-stable.list
sudo apt update
sudo apt install caddy
```

Проверить service:

```bash
systemctl status caddy
```

### 18.3 Настроить Caddyfile для нового домена

Открыть конфиг:

```bash
sudo nano /etc/caddy/Caddyfile
```

Конфиг с отдельной обработкой SSE `/api/events`:

```caddyfile
api.your-domain.com {
    encode zstd gzip

    @sse path /api/events
    handle @sse {
        reverse_proxy 127.0.0.1:8787 {
            # Keep EventSource streams alive across long dashboard sessions.
            stream_timeout 24h
            stream_close_delay 5m

            # Extra hints for clients/intermediate proxies.
            header_down Cache-Control "no-cache"
            header_down X-Accel-Buffering "no"
        }
    }

    handle {
        reverse_proxy 127.0.0.1:8787
    }
}
```

Заменить `api.your-domain.com` на реальный домен.

Если GEM_RUST запущен с `--server-bind 0.0.0.0:8787`, Caddy все равно нормально ходит на `127.0.0.1:8787`: `0.0.0.0` слушает все интерфейсы, включая localhost.

`flush_interval -1` обычно не нужен: Caddy flush-ит `text/event-stream` ответы сразу. Если когда-нибудь увидишь, что `/api/events` копит события пачками, можно добавить внутрь SSE `reverse_proxy`:

```caddyfile
flush_interval -1
```

Caddy сам получит и будет продлевать HTTPS-сертификат, если:

```text
[ ] DNS A record уже указывает на VPS IP
[ ] ports 80 и 443 открыты снаружи
[ ] Caddy запущен и может слушать 80/443
```

### 18.4 Опционально позже: перебиндить GEM_RUST на localhost

Этот шаг **не нужен для текущего Caddy setup**. Делать только если позже решишь закрыть прямой внешний `8787`.

Открыть systemd service:

```bash
sudo nano /etc/systemd/system/gem-rust-sweden-btc-5m.service
```

В `ExecStart` заменить:

```text
--server-bind 0.0.0.0:8787
```

на:

```text
--server-bind 127.0.0.1:8787
```

Применить:

```bash
sudo systemctl daemon-reload
sudo systemctl restart gem-rust-sweden-btc-5m
sudo systemctl status gem-rust-sweden-btc-5m
```

### 18.5 Проверить и перезагрузить Caddy

Проверить синтаксис:

```bash
sudo caddy fmt --overwrite /etc/caddy/Caddyfile
sudo caddy validate --config /etc/caddy/Caddyfile
```

Применить конфиг без downtime:

```bash
sudo systemctl reload caddy
```

Если нужен полный restart:

```bash
sudo systemctl restart caddy
sudo systemctl status caddy
```

Логи Caddy:

```bash
journalctl -u caddy -f
```

Проверка API через домен:

```bash
curl -sS https://api.your-domain.com/api/health
curl -sS https://api.your-domain.com/api/state | head -c 1000
```

Если UI на Render должен ходить напрямую в API, указать API URL:

```text
https://api.your-domain.com
```

Если UI использует same-origin `/api/*` proxy, проксировать `/api/*` на:

```text
https://api.your-domain.com/api/*
```
