#!/usr/bin/env bash
# Pasang / perbarui mudah-rs sebagai service systemd di /opt/mudah-rs.
# Jalankan dari root project, sesudah: cargo build --release --bin mudah-rs
#   sudo deploy/install-systemd.sh
set -euo pipefail

APP_DIR=/opt/mudah-rs
APP_USER=mudah
BIN=target/release/mudah-rs
UNIT=deploy/mudah-rs.service

[[ $EUID -eq 0 ]] || { echo "Jalankan dengan sudo." >&2; exit 1; }
[[ -f $BIN ]] || { echo "$BIN belum ada. Build dulu: cargo build --release --bin mudah-rs" >&2; exit 1; }
[[ -f $UNIT ]] || { echo "Jalankan dari root project ($UNIT tidak ketemu)." >&2; exit 1; }

# User sistem
id -u "$APP_USER" >/dev/null 2>&1 || \
  useradd --system --home "$APP_DIR" --shell /usr/sbin/nologin "$APP_USER"

mkdir -p "$APP_DIR/logs"

# Binary (versi lama disimpan sebagai mudah-rs.prev untuk rollback)
install -o "$APP_USER" -g "$APP_USER" -m 755 "$BIN" "$APP_DIR/mudah-rs.new"
[[ -f $APP_DIR/mudah-rs ]] && cp -p "$APP_DIR/mudah-rs" "$APP_DIR/mudah-rs.prev"
mv "$APP_DIR/mudah-rs.new" "$APP_DIR/mudah-rs"

# Template email
rm -rf "$APP_DIR/templates"
cp -r templates "$APP_DIR/"

# .env hanya disalin bila belum ada (tidak menimpa .env server)
if [[ ! -f $APP_DIR/.env ]]; then
  [[ -f .env ]] || { echo ".env tidak ada di project maupun $APP_DIR." >&2; exit 1; }
  cp .env "$APP_DIR/.env"
  echo "Menyalin .env ke $APP_DIR/.env, periksa isinya."
fi
chmod 600 "$APP_DIR/.env"

chown -R "$APP_USER:$APP_USER" "$APP_DIR"

# Migrasi + script backup/restore database (db/migrate.sh, db/backup-db.sh) di
# direktori TERPISAH milik root, bukan di $APP_DIR: backup-db.sh dijalankan cron root,
# dan $APP_DIR milik $APP_USER (bisa mengganti isinya). Koneksi & BACKUP_DIR dibaca dari
# /etc/mudah-api-backup.conf (lihat db/backup-db.sh).
DB_DIR=/opt/mudah-db
install -d -o root -g root -m 755 "$DB_DIR"
rm -rf "$DB_DIR/migrations" "$DB_DIR/lib"
cp -r db/. "$DB_DIR/"
chown -R root:root "$DB_DIR"
chmod 755 "$DB_DIR"/*.sh

# Unit systemd
install -m 644 "$UNIT" /etc/systemd/system/mudah-rs.service
systemctl daemon-reload
systemctl enable mudah-rs >/dev/null
systemctl restart mudah-rs

sleep 2
systemctl --no-pager status mudah-rs | head -n 5
echo "Log: journalctl -u mudah-rs -f"
