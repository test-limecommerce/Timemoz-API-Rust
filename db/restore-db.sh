#!/usr/bin/env bash
#
# restore-db.sh — Restore dump PostgreSQL hasil backup-db.sh.
#
# DEFAULT-NYA AMAN: restore masuk ke database BARU, bukan menimpa yang lama.
# Kamu bisa memeriksa hasilnya dulu, baru mengarahkan aplikasi ke sana.
# Menimpa database yang sudah ada harus diminta eksplisit dengan --overwrite,
# dan script akan mengambil dump pengaman lebih dulu.
#
# Pemakaian:
#   db/restore-db.sh --list  /var/backups/mudah-api/timemoz-20260821-010000.dump
#   db/restore-db.sh         /var/backups/mudah-api/timemoz-20260821-010000.dump
#   db/restore-db.sh --to timemoz_staging  <file.dump>
#   db/restore-db.sh --overwrite --to timemoz  <file.dump>     # DESTRUKTIF
#
# Opsi:
#   --to <nama>     Nama database tujuan. Default: <db-asal>_restore_<timestamp>
#   --overwrite     Restore ke database yang SUDAH ADA (isinya diganti).
#   --jobs <n>      Job paralel pg_restore. Default 2 (droplet 2 vCPU).
#   --list          Tampilkan isi dump lalu keluar. Tidak menyentuh database.
#   --yes           Lewati konfirmasi interaktif (untuk otomasi).
#   -h, --help      Bantuan ini.
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

# Cetak blok komentar header sebagai bantuan — berhenti di baris non-komentar
# pertama, jadi tidak ikut mencetak kode.
usage() { awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "${BASH_SOURCE[0]}"; }

# ---------------------------------------------------------------------------
# Argumen
# ---------------------------------------------------------------------------
DUMP_FILE=""
TARGET_DB=""
OVERWRITE=0
LIST_ONLY=0
ASSUME_YES=0
JOBS="${JOBS:-2}"

while (( $# )); do
  case "$1" in
    --to)        TARGET_DB="${2:-}"; shift 2 ;;
    --overwrite) OVERWRITE=1; shift ;;
    --jobs)      JOBS="${2:-}"; shift 2 ;;
    --list)      LIST_ONLY=1; shift ;;
    --yes|-y)    ASSUME_YES=1; shift ;;
    -h|--help)   usage; exit 0 ;;
    -*)          echo "Opsi tidak dikenal: $1" >&2; usage >&2; exit 2 ;;
    *)
      if [[ -n "${DUMP_FILE}" ]]; then
        echo "File dump hanya boleh satu (dapat: ${DUMP_FILE} dan $1)" >&2; exit 2
      fi
      DUMP_FILE="$1"; shift ;;
  esac
done

if [[ -z "${DUMP_FILE}" ]]; then
  echo "File dump wajib diisi." >&2; usage >&2; exit 2
fi

if [[ ! "${JOBS}" =~ ^[1-9][0-9]?$ ]]; then
  echo "--jobs harus angka 1-99, dapat: ${JOBS}" >&2; exit 2
fi

# ---------------------------------------------------------------------------
# Config & koneksi
# ---------------------------------------------------------------------------
BACKUP_CONF="${BACKUP_CONF:-/etc/mudah-api-backup.conf}"
load_backup_conf

ENV_FILE="${ENV_FILE:-${PROJECT_ROOT}/.env}"
BACKUP_DIR="${BACKUP_DIR:-${PROJECT_ROOT}/backups/postgres}"
LOG_FILE="${LOG_FILE:-${BACKUP_DIR}/restore.log}"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"

mkdir -p "${BACKUP_DIR}" 2>/dev/null || true
touch "${LOG_FILE}" 2>/dev/null || LOG_FILE=""

trap 'err "Restore GAGAL di baris ${BASH_LINENO[0]}."; exit 1' ERR
trap 'err "Dihentikan oleh sinyal."; exit 143' INT TERM

info "=== Mulai restore ==="
[[ -n "${CONF_LOADED:-}" ]] && info "Config dimuat dari ${BACKUP_CONF}"

# ---------------------------------------------------------------------------
# Validasi file dump — dilakukan SEBELUM menyentuh database sama sekali
# ---------------------------------------------------------------------------
if [[ ! -r "${DUMP_FILE}" ]]; then
  err "File dump tidak bisa dibaca: ${DUMP_FILE}"
  exit 1
fi

require_pg_clients pg_restore psql pg_dump

if ! pg_restore --list "${DUMP_FILE}" >/dev/null 2>&1; then
  err "File bukan arsip pg_dump format custom yang valid, atau rusak."
  err "Kalau ini dump .sql polos, pakai: psql -d <db> -f ${DUMP_FILE}"
  exit 1
fi

# Cocokkan dengan checksum pendamping kalau ada — mendeteksi file yang rusak
# di disk atau terpotong saat disalin dari B2.
if [[ -r "${DUMP_FILE}.sha256" ]] && command -v sha256sum >/dev/null; then
  if (cd "$(dirname "${DUMP_FILE}")" && sha256sum -c --status "$(basename "${DUMP_FILE}").sha256"); then
    info "Checksum SHA256 cocok."
  else
    err "Checksum SHA256 TIDAK cocok — file dump rusak atau tidak lengkap."
    err "Jangan dipakai restore. Ambil ulang dari B2."
    exit 1
  fi
else
  warn "Tidak ada file .sha256 pendamping — integritas file tidak diverifikasi."
fi

TABLE_COUNT="$(pg_restore --list "${DUMP_FILE}" | grep -c 'TABLE DATA' || true)"
info "Dump valid: ${TABLE_COUNT} tabel berisi data, $(du -h "${DUMP_FILE}" | cut -f1)"

if (( LIST_ONLY )); then
  info "Mode --list, isi dump:"
  pg_restore --list "${DUMP_FILE}"
  info "=== Selesai (tidak ada perubahan pada database) ==="
  exit 0
fi

resolve_database_url
export_pg_env
info "Server: ${PGUSER}@${PGHOST}:${PGPORT} (database asal: ${SRC_DB_NAME})"

check_server_version pg_restore

# ---------------------------------------------------------------------------
# Tentukan database tujuan
# ---------------------------------------------------------------------------
if [[ -z "${TARGET_DB}" ]]; then
  # Tanda hubung di TIMESTAMP diganti underscore: identifier Postgres tanpa
  # tanda kutip tidak boleh mengandung '-', dan validasi di bawah menolaknya.
  TARGET_DB="${SRC_DB_NAME}_restore_${TIMESTAMP//-/_}"
  # Nama database Postgres maksimal 63 byte.
  TARGET_DB="${TARGET_DB:0:63}"
fi

# Divalidasi ketat karena nama ini masuk ke DDL yang tidak bisa diparameterkan.
if [[ ! "${TARGET_DB}" =~ ^[A-Za-z_][A-Za-z0-9_]{0,62}$ ]]; then
  err "Nama database tidak valid: '${TARGET_DB}'"
  err "Hanya huruf, angka, dan underscore; tidak boleh diawali angka."
  exit 1
fi

# Database perawatan untuk menjalankan CREATE DATABASE — tidak bisa dijalankan
# dari dalam database yang sedang dibuat. DO memakai 'defaultdb'.
MAINT_DB=""
for candidate in "${MAINT_DB_OVERRIDE:-}" defaultdb postgres "${SRC_DB_NAME}"; do
  [[ -z "${candidate}" ]] && continue
  if psql -d "${candidate}" -tAX -c 'SELECT 1' >/dev/null 2>&1; then
    MAINT_DB="${candidate}"
    break
  fi
done
if [[ -z "${MAINT_DB}" ]]; then
  err "Tidak menemukan database perawatan (coba: defaultdb, postgres, ${SRC_DB_NAME})."
  exit 1
fi
info "Database perawatan: ${MAINT_DB}"

TARGET_EXISTS=0
if [[ "$(psql -d "${MAINT_DB}" -tAX -c "SELECT 1 FROM pg_database WHERE datname = '${TARGET_DB}'")" == "1" ]]; then
  TARGET_EXISTS=1
fi

# ---------------------------------------------------------------------------
# Konfirmasi
# ---------------------------------------------------------------------------
confirm() {
  local prompt="$1" expected="$2" answer
  if (( ASSUME_YES )); then
    warn "--yes dipakai, konfirmasi dilewati."
    return 0
  fi
  if [[ ! -t 0 && ! -r /dev/tty ]]; then
    err "Butuh konfirmasi interaktif tapi tidak ada terminal. Pakai --yes kalau memang disengaja."
    exit 1
  fi
  printf '%s\n' "${prompt}" >&2
  read -r -p "Ketik '${expected}' untuk lanjut: " answer < /dev/tty
  [[ "${answer}" == "${expected}" ]]
}

if (( TARGET_EXISTS )); then
  if (( ! OVERWRITE )); then
    err "Database '${TARGET_DB}' sudah ada."
    err "Pakai --to <nama-lain> untuk restore ke database baru,"
    err "atau --overwrite kalau memang ingin mengganti isinya."
    exit 1
  fi

  echo >&2
  warn "!!! Akan MENIMPA database '${TARGET_DB}' yang sudah ada di ${PGHOST}"
  warn "!!! Seluruh tabel di dalamnya akan di-DROP lalu dibuat ulang dari dump."
  if ! confirm "Ini tidak bisa dibatalkan." "${TARGET_DB}"; then
    err "Konfirmasi tidak cocok. Dibatalkan, tidak ada yang diubah."
    exit 1
  fi

  # Sabuk pengaman: dump dulu kondisi sekarang, supaya kalau restore-nya
  # ternyata salah file, masih ada jalan mundur.
  SAFETY_DUMP="${BACKUP_DIR}/${TARGET_DB}-preRestore-${TIMESTAMP}.dump"
  info "Mengambil dump pengaman dulu -> $(basename "${SAFETY_DUMP}")"
  if pg_dump -d "${TARGET_DB}" --format=custom --compress=6 \
       --no-owner --no-privileges --file="${SAFETY_DUMP}"; then
    chmod 600 "${SAFETY_DUMP}" 2>/dev/null || true
    info "Dump pengaman siap: ${SAFETY_DUMP} ($(du -h "${SAFETY_DUMP}" | cut -f1))"
  else
    err "Dump pengaman GAGAL. Restore dibatalkan — tidak mau menimpa tanpa jalan mundur."
    rm -f "${SAFETY_DUMP}"
    exit 1
  fi
else
  info "Membuat database baru '${TARGET_DB}'"
  # template0 supaya bersih dari objek bawaan template1 yang bisa bentrok.
  psql -d "${MAINT_DB}" -v ON_ERROR_STOP=1 -c \
    "CREATE DATABASE \"${TARGET_DB}\" TEMPLATE template0 ENCODING 'UTF8'" >/dev/null
  info "Database '${TARGET_DB}' dibuat."
fi

# ---------------------------------------------------------------------------
# Restore
# ---------------------------------------------------------------------------
RESTORE_LOG="$(mktemp)"
trap 'rm -f "${RESTORE_LOG}"' EXIT

RESTORE_ARGS=(--dbname "${TARGET_DB}" --no-owner --no-privileges --jobs "${JOBS}")
(( TARGET_EXISTS )) && RESTORE_ARGS+=(--clean --if-exists)

info "Menjalankan pg_restore (${JOBS} job paralel) -> ${TARGET_DB}"
RESTORE_RC=0
pg_restore "${RESTORE_ARGS[@]}" "${DUMP_FILE}" >"${RESTORE_LOG}" 2>&1 || RESTORE_RC=$?

ERROR_COUNT="$(grep -c '^pg_restore: error:' "${RESTORE_LOG}" || true)"

if (( ERROR_COUNT > 0 )); then
  err "pg_restore melaporkan ${ERROR_COUNT} error. Sepuluh pertama:"
  grep '^pg_restore: error:' "${RESTORE_LOG}" | head -10 | while IFS= read -r line; do
    err "  ${line}"
  done
  SAVED_LOG="${BACKUP_DIR}/restore-${TARGET_DB}-${TIMESTAMP}.log"
  if cp "${RESTORE_LOG}" "${SAVED_LOG}" 2>/dev/null; then
    err "Log lengkap pg_restore disimpan di: ${SAVED_LOG}"
  fi
  err "Catatan: 'must be owner of extension' biasanya tidak berbahaya di managed"
  err "database — doadmin bukan superuser. Error selain itu perlu diperiksa."
elif (( RESTORE_RC != 0 )); then
  err "pg_restore keluar dengan status ${RESTORE_RC} tanpa baris error yang terbaca."
else
  info "pg_restore selesai tanpa error."
fi

# ---------------------------------------------------------------------------
# Verifikasi hasil
# ---------------------------------------------------------------------------
info "--- Hasil di database '${TARGET_DB}' ---"
psql -d "${TARGET_DB}" -tAX -c "
  SELECT 'Tabel di schema public: ' || count(*)
  FROM information_schema.tables
  WHERE table_schema = 'public' AND table_type = 'BASE TABLE'
" | while IFS= read -r line; do info "${line}"; done

psql -d "${TARGET_DB}" -tAX -c \
  "SELECT 'Ukuran database: ' || pg_size_pretty(pg_database_size(current_database()))" \
  | while IFS= read -r line; do info "${line}"; done

info "Lima tabel terbesar:"
psql -d "${TARGET_DB}" -tAX -F' | ' -c "
  SELECT relname, pg_size_pretty(pg_total_relation_size(c.oid))
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE c.relkind = 'r' AND n.nspname = 'public'
  ORDER BY pg_total_relation_size(c.oid) DESC
  LIMIT 5
" | while IFS= read -r line; do info "  ${line}"; done

echo >&2
if (( ERROR_COUNT > 0 || RESTORE_RC != 0 )); then
  err "=== Restore selesai DENGAN ERROR — periksa dulu sebelum dipakai ==="
  exit 1
fi

info "Restore sukses ke database: ${TARGET_DB}"
if (( ! TARGET_EXISTS )); then
  info "Database produksi '${SRC_DB_NAME}' TIDAK disentuh."
  info "Untuk memakainya, arahkan DATABASE_URL aplikasi ke /${TARGET_DB}"
  info "Kalau tidak jadi dipakai, hapus dengan:"
  info "  psql -d ${MAINT_DB} -c 'DROP DATABASE \"${TARGET_DB}\"'"
fi
info "=== Selesai ==="
