
RUN:
cargo run -- BTC 5m --server
cargo run -- BTC 5m --live --dry-run --server
cargo run -- BTC 5m --live --server


статус
cargo run -- --server --status


Стоп (graceful до 25с, потом kill)
cargo run -- --server --stop

Убить сразу
cargo run -- --server --stop --force


UI (отдельно)
cd ../GEM_RUST_UI && npm run dev
http://127.0.0.1:5173


BIND
127.0.0.1:8787
