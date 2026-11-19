#!/usr/bin/env bash
#
# backup-db.sh — Backup harian PostgreSQL untuk database Mudah (Timemoz).
#
# Alur:
#   1. Baca MIGRATION_DATABASE_URL, atau DATABASE_URL kalau yang pertama tidak
#      ada — prioritas masing-masing: environment > /etc/mudah-api-backup.conf
#      > .env di root project. URL connection pool DigitalOcean (port 25061)
#      ditolak: pg_dump tidak bisa lewat PgBouncer, jadi harus koneksi langsung.
#   2. pg_dump ke format custom (-Fc) yang terkompresi.
#   3. Verifikasi hasil dump (pg_restore --list) — dump rusak = job gagal.
#   4. Opsional: upload off-site ke Backblaze B2 (aktif kalau B2_* diisi).
#   5. HANYA kalau backup hari ini sukses & terverifikasi: hapus backup lokal
#      yang lebih tua dari RETENTION_DAYS (default 7 hari).
#
# Config di server (/etc/mudah-api-backup.conf, chown root:root, chmod 600):
#   # Koneksi LANGSUNG (port 25060, database asli — bukan port/nama pool 25061).
#   # Dipakai juga oleh db/migrate.sh.
#   MIGRATION_DATABASE_URL="postgresql://doadmin:xxx@db-...ondigitalocean.com:25060/timemoz?sslmode=require"
#   BACKUP_DIR=/var/backups/mudah-api
#   B2_KEY_ID="..."          # kosongkan untuk mematikan upload off-site
#   B2_APP_KEY="..."
#   B2_BUCKET_ID="..."
#   B2_PREFIX="mudah-api"    # opsional, folder di dalam bucket
#
# Pemakaian:
#   db/backup-db.sh
#   BACKUP_DIR=/mnt/backup RETENTION_DAYS=14 db/backup-db.sh
#   DRY_RUN=1 db/backup-db.sh        # tampilkan yang akan dihapus saja
#
# Cron produksi (server UTC, jadi zona waktu dinyatakan eksplisit):
#   CRON_TZ=Asia/Jakarta
#   TZ=Asia/Jakarta
#   0 1 * * * /opt/mudah-db/backup-db.sh >/dev/null 2>&1
#
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# Root project = induk db/ di checkout repo; di server (db/ dipasang sendirian di
# /opt/mudah-db oleh deploy/install-systemd.sh) = direktori script ini.
if [[ -f "${SCRIPT_DIR}/../Cargo.toml" ]]; then
  PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
else
  PROJECT_ROOT="${SCRIPT_DIR}"
fi
# shellcheck source=lib/pg-common.sh
source "${SCRIPT_DIR}/lib/pg-common.sh"

BACKUP_CONF="${BACKUP_CONF:-/etc/mudah-api-backup.conf}"
load_backup_conf

ENV_FILE="${ENV_FILE:-${PROJECT_ROOT}/.env}"
BACKUP_DIR="${BACKUP_DIR:-${PROJECT_ROOT}/backups/postgres}"
RETENTION_DAYS="${RETENTION_DAYS:-7}"
LOG_FILE="${LOG_FILE:-${BACKUP_DIR}/backup.log}"
LOCK_FILE="${LOCK_FILE:-/tmp/mudah-api-backup-db.lock}"
MIN_DUMP_BYTES="${MIN_DUMP_BYTES:-1024}"   # dump di bawah ini dianggap gagal
COMPRESS_LEVEL="${COMPRESS_LEVEL:-6}"      # kompresi jalan di CPU droplet
DRY_RUN="${DRY_RUN:-0}"

TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
TODAY="$(date +%Y-%m-%d)"

on_error() {
  local exit_code=$?
  err "Backup GAGAL di baris ${BASH_LINENO[0]} (exit ${exit_code}). Backup lama TIDAK dihapus."
  exit "${exit_code}"
}

# Cleanup jalan untuk SEMUA jalur keluar (error, exit eksplisit, maupun sinyal),
# supaya dump setengah jadi tidak pernah tertinggal di direktori backup.
on_exit() {
  local exit_code=$?
  if [[ -n "${DUMP_TMP:-}" && -f "${DUMP_TMP}" ]]; then
    rm -f "${DUMP_TMP}"
    warn "Dump tidak selesai dibersihkan: $(basename "${DUMP_TMP}")"
  fi
  (( exit_code != 0 )) && err "Backup diakhiri dengan status ${exit_code}."
  return 0
}
trap on_error ERR
trap on_exit EXIT
trap 'err "Dihentikan oleh sinyal."; exit 143' INT TERM

mkdir -p "${BACKUP_DIR}"
chmod 700 "${BACKUP_DIR}" 2>/dev/null || true
touch "${LOG_FILE}"
chmod 600 "${LOG_FILE}" 2>/dev/null || true

# Cegah dua job cron jalan bersamaan (backup sebelumnya belum selesai).
exec 9>"${LOCK_FILE}"
if ! flock -n 9; then
  warn "Proses backup lain masih berjalan (lock: ${LOCK_FILE}). Keluar."
  exit 0
fi

info "=== Mulai backup database (${TODAY}) ==="
[[ -n "${CONF_LOADED:-}" ]] && info "Config dimuat dari ${BACKUP_CONF}"

resolve_direct_database_url
export_pg_env
reject_do_pool_port
info "Target: ${PGUSER}@${PGHOST}:${PGPORT}/${PGDATABASE} (dari ${URL_SOURCE})"

require_pg_clients pg_dump pg_restore psql
check_server_version pg_dump

# ---------------------------------------------------------------------------
# 1. Dump ke file sementara
# ---------------------------------------------------------------------------
DUMP_NAME="${PGDATABASE}-${TIMESTAMP}.dump"
DUMP_PATH="${BACKUP_DIR}/${DUMP_NAME}"
DUMP_TMP="${DUMP_PATH}.part"

info "Menjalankan pg_dump -> ${DUMP_NAME} (kompresi ${COMPRESS_LEVEL})"
pg_dump \
  --format=custom \
  --compress="${COMPRESS_LEVEL}" \
  --no-owner \
  --no-privileges \
  --file="${DUMP_TMP}"

# ---------------------------------------------------------------------------
# 2. Verifikasi hasil dump sebelum dianggap sukses
# ---------------------------------------------------------------------------
dump_size="$(stat -c %s "${DUMP_TMP}")"
if (( dump_size < MIN_DUMP_BYTES )); then
  err "Ukuran dump mencurigakan (${dump_size} byte < ${MIN_DUMP_BYTES})."
  exit 1
fi

if ! pg_restore --list "${DUMP_TMP}" >/dev/null 2>&1; then
  err "Dump tidak lolos verifikasi pg_restore --list — file kemungkinan rusak."
  exit 1
fi

mv -f "${DUMP_TMP}" "${DUMP_PATH}"
chmod 600 "${DUMP_PATH}"
unset DUMP_TMP

# Checksum untuk audit / cek integritas saat restore.
if command -v sha256sum >/dev/null; then
  (cd "${BACKUP_DIR}" && sha256sum "${DUMP_NAME}" > "${DUMP_NAME}.sha256")
  chmod 600 "${DUMP_PATH}.sha256"
fi

info "Backup sukses: ${DUMP_PATH} ($(du -h "${DUMP_PATH}" | cut -f1))"

# ---------------------------------------------------------------------------
# 3. Upload off-site ke Backblaze B2 (opsional)
#
# Backup lokal satu droplet dengan aplikasi; kalau droplet hilang, backupnya ikut
# hilang. Upload ini yang membuatnya benar-benar off-site.
#
# Kegagalan upload TIDAK membatalkan retensi lokal — dump lokal sudah
# terverifikasi dan valid — tapi script exit 1 supaya kegagalannya terlihat.
# ---------------------------------------------------------------------------
B2_KEY_ID="${B2_KEY_ID:-}"
B2_APP_KEY="${B2_APP_KEY:-}"
B2_BUCKET_ID="${B2_BUCKET_ID:-}"
B2_PREFIX="${B2_PREFIX:-mudah-api}"
B2_RETRIES="${B2_RETRIES:-3}"
B2_RETRY_DELAY="${B2_RETRY_DELAY:-5}"
B2_UPLOAD_TIMEOUT="${B2_UPLOAD_TIMEOUT:-1800}"
B2_MAX_SINGLE_PART=5000000000     # batas b2_upload_file sekali jalan
UPLOAD_FAILED=0

b2_upload() {
  local file="$1" remote_name="$2"
  local size sha1 attempt auth api_url auth_token upload_data upload_url upload_token
  local resp file_id resp_sha

  size="$(stat -c %s "${file}")"
  if (( size > B2_MAX_SINGLE_PART )); then
    err "Ukuran ${size} byte melebihi batas upload sekali jalan B2 (5 GB)."
    err "Perlu large-file API (b2_start_large_file) — belum diimplementasikan."
    return 1
  fi

  sha1="$(sha1sum "${file}" | cut -d " " -f1)"

  for (( attempt = 1; attempt <= B2_RETRIES; attempt++ )); do
    if (( attempt > 1 )); then
      warn "Upload B2 percobaan ${attempt}/${B2_RETRIES}, jeda ${B2_RETRY_DELAY}s"
      sleep "${B2_RETRY_DELAY}"
    fi

    # Token upload B2 bisa kedaluwarsa/ditolak, dan cara yang dianjurkan adalah
    # mengulang dari authorize, jadi seluruh rangkaian ada di dalam loop retry.
    if ! auth="$(curl -sS --max-time 60 \
        -u "${B2_KEY_ID}:${B2_APP_KEY}" \
        "https://api.backblazeb2.com/b2api/v3/b2_authorize_account" 2>&1)"; then
      warn "b2_authorize_account tidak bisa dihubungi: ${auth}"
      continue
    fi

    # Dicek dengan keberadaan field yang DIBUTUHKAN, bukan dengan membandingkan
    # field .status ke string "null" — pengecekan itu pecah kalau respons kosong.
    api_url="$(jq -r ".apiInfo.storageApi.apiUrl // .apiUrl // empty" <<< "${auth}" 2>/dev/null || true)"
    auth_token="$(jq -r ".authorizationToken // empty" <<< "${auth}" 2>/dev/null || true)"
    if [[ -z "${api_url}" || -z "${auth_token}" ]]; then
      warn "Autentikasi B2 ditolak: $(jq -rc "{status,code,message}" <<< "${auth}" 2>/dev/null || echo "respons tidak terbaca")"
      continue
    fi

    if ! upload_data="$(curl -sS --max-time 60 \
        -H "Authorization: ${auth_token}" \
        "${api_url}/b2api/v3/b2_get_upload_url?bucketId=${B2_BUCKET_ID}" 2>&1)"; then
      warn "b2_get_upload_url tidak bisa dihubungi: ${upload_data}"
      continue
    fi

    upload_url="$(jq -r ".uploadUrl // empty" <<< "${upload_data}" 2>/dev/null || true)"
    upload_token="$(jq -r ".authorizationToken // empty" <<< "${upload_data}" 2>/dev/null || true)"
    if [[ -z "${upload_url}" || -z "${upload_token}" ]]; then
      warn "Gagal dapat upload url: $(jq -rc "{status,code,message}" <<< "${upload_data}" 2>/dev/null || echo "respons tidak terbaca")"
      continue
    fi

    # -T (bukan --data-binary) supaya curl STREAMING dari disk; --data-binary
    # memuat seluruh file ke RAM, dan itu tidak perlu di droplet 2 GB.
    # -H "Expect:" mematikan handshake 100-continue yang cuma menambah latensi.
    info "Upload ke B2: ${remote_name} ($(du -h "${file}" | cut -f1))"
    if ! resp="$(curl -sS --max-time "${B2_UPLOAD_TIMEOUT}" -X POST \
        -H "Authorization: ${upload_token}" \
        -H "X-Bz-File-Name: ${remote_name}" \
        -H "Content-Type: application/octet-stream" \
        -H "X-Bz-Content-Sha1: ${sha1}" \
        -H "Expect:" \
        -T "${file}" \
        "${upload_url}" 2>&1)"; then
      warn "Upload terputus: ${resp}"
      continue
    fi

    file_id="$(jq -r ".fileId // empty" <<< "${resp}" 2>/dev/null || true)"
    resp_sha="$(jq -r ".contentSha1 // empty" <<< "${resp}" 2>/dev/null || true)"

    if [[ -z "${file_id}" ]]; then
      warn "Upload ditolak B2: $(jq -rc "{status,code,message}" <<< "${resp}" 2>/dev/null || echo "${resp}")"
      continue
    fi

    # B2 mengembalikan SHA1 yang IA hitung dari byte yang diterima. Kalau cocok,
    # file di sana identik dengan yang di sini — bukan sekadar "HTTP 200".
    if [[ "${resp_sha}" != "${sha1}" ]]; then
      err "SHA1 tidak cocok — lokal ${sha1}, di B2 ${resp_sha}. File di B2 rusak."
      return 1
    fi

    info "Upload B2 sukses: ${remote_name} (fileId ${file_id})"
    return 0
  done

  return 1
}

if [[ -n "${B2_KEY_ID}" && -n "${B2_APP_KEY}" && -n "${B2_BUCKET_ID}" ]]; then
  if ! command -v jq >/dev/null; then
    err "jq dibutuhkan untuk upload B2. Pasang: sudo apt install -y jq"
    UPLOAD_FAILED=1
  elif ! b2_upload "${DUMP_PATH}" "${B2_PREFIX}/${DUMP_NAME}"; then
    err "Upload ke Backblaze B2 GAGAL (lihat WARN/ERROR di atas untuk sebabnya)."
    err "Backup lokal tetap aman di ${DUMP_PATH}."
    UPLOAD_FAILED=1
  fi
else
  info "Config B2 tidak lengkap — upload off-site dilewati."
fi

# ---------------------------------------------------------------------------
# 4. Retention lokal — baru dijalankan setelah backup hari ini terbukti sukses
# ---------------------------------------------------------------------------
finish() {
  if (( UPLOAD_FAILED )); then
    err "Backup lokal sukses, tetapi upload off-site GAGAL."
    exit 1
  fi
  info "=== Selesai ==="
  exit 0
}

if (( RETENTION_DAYS < 1 )); then
  warn "RETENTION_DAYS=${RETENTION_DAYS}, pembersihan dilewati."
  finish
fi

# Sabuk pengaman: pastikan file hari ini benar-benar ada di direktori target
# sebelum menghapus apa pun.
if [[ ! -s "${DUMP_PATH}" ]]; then
  err "File backup hari ini tidak ditemukan setelah dump. Pembersihan dibatalkan."
  exit 1
fi

info "Membersihkan backup lebih tua dari ${RETENTION_DAYS} hari di ${BACKUP_DIR}"

deleted=0
while IFS= read -r -d '' old_file; do
  if [[ "${DRY_RUN}" == "1" ]]; then
    info "[DRY RUN] akan dihapus: ${old_file}"
  else
    rm -f "${old_file}" "${old_file}.sha256"
    info "Dihapus: $(basename "${old_file}")"
  fi
  deleted=$(( deleted + 1 ))
done < <(find "${BACKUP_DIR}" -maxdepth 1 -type f -name "*.dump" \
           ! -newermt "-${RETENTION_DAYS} days" -print0)

# Bersihkan file .part yatim (sisa dump yang mati di tengah jalan).
if [[ "${DRY_RUN}" != "1" ]]; then
  find "${BACKUP_DIR}" -maxdepth 1 -type f -name "*.dump.part" -mmin +720 -delete 2>/dev/null || true
  find "${BACKUP_DIR}" -maxdepth 1 -type f -name "*.sha256" \
    ! -newermt "-${RETENTION_DAYS} days" -delete 2>/dev/null || true
fi

remaining="$(find "${BACKUP_DIR}" -maxdepth 1 -type f -name '*.dump' | wc -l)"
if [[ "${DRY_RUN}" == "1" ]]; then
  info "[DRY RUN] Selesai: ${deleted} file akan dihapus, ${remaining} backup ada sekarang."
else
  info "Pembersihan selesai: ${deleted} file dihapus, ${remaining} backup tersisa."
fi
finish
