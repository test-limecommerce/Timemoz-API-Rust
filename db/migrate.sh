#!/usr/bin/env bash
#
# migrate.sh — menerapkan satu migration.sql ke database dengan aman.
#
# Port `scripts/apply-migration.sh` Mudah-API (staging 1d581b8). Sejak migrasi
# pindah ke repo ini, skema database dimiliki db/ (bukan lagi prisma/ di repo
# Node) dan TIDAK ada langkah yang butuh Node/npm: hanya bash, psql, pg_dump.
# Tambahan dibanding versi Node: catatan migrasi (_db_migrations) dan blokir
# perintah yang bisa menghapus/menimpa data.
#
# Alur:
#   1. Migration dipilih: default = folder TERBARU di db/migrations; bisa
#      juga sebagian nama (mis. `divisions`). Tetap SATU migration per jalan,
#      tanpa memaksa urutan (migration lebih lama yang belum tercatat hanya
#      diperingatkan).
#   2. Blokir data: migration yang berisi DROP TABLE/COLUMN/SCHEMA/TYPE/
#      SEQUENCE/DATABASE, TRUNCATE, DELETE FROM, UPDATE … SET (termasuk
#      ON CONFLICT DO UPDATE SET), ALTER COLUMN … TYPE, atau CASCADE (selain
#      ON DELETE/UPDATE CASCADE) DITOLAK sebelum apa pun dijalankan, kecuali
#      dengan --allow-destructive. Di produksi masih harus mengetik HAPUS.
#   3. URL database: MIGRATION_DATABASE_URL kalau ada, selain itu DATABASE_URL.
#      Masing-masing dicari di environment > /etc/mudah-api-backup.conf > .env
#      di root project (?schema=public dibuang otomatis).
#      MIGRATION_DATABASE_URL = koneksi LANGSUNG ke Postgres (DigitalOcean: port
#      25060, database asli, bukan nama pool). pg_dump tidak bisa lewat
#      connection pool PgBouncer (port 25061), jadi URL port 25061 ditolak
#      sebelum apa pun dijalankan. Aplikasi tetap memakai DATABASE_URL (pool).
#   4. Catatan migrasi: tabel "_db_migrations" (nama, checksum sha256 file,
#      waktu, user, mode). Migration yang SUDAH tercatat ditolak (menjalankan
#      ulang bisa mengubah data lagi tanpa error), kecuali --force. Database
#      tanpa catatan ditolak sampai di-baseline sekali (--baseline).
#   5. Konfirmasi. Database produksi (host *.db.ondigitalocean.com atau
#      NODE_ENV=production) wajib diketik nama database-nya; --yes diabaikan.
#   6. Backup pg_dump (format custom) ke backups/migrations/, diverifikasi
#      dengan pg_restore --list. Backup gagal = migration TIDAK dijalankan.
#   7. psql --single-transaction -v ON_ERROR_STOP=1: migration + baris
#      catatannya dalam SATU transaksi; satu error = semua batal.
#      Kecuali migration berisi CONCURRENTLY (CREATE/DROP INDEX CONCURRENTLY),
#      yang ditolak Postgres di dalam transaksi: file itu dijalankan TANPA
#      --single-transaction dan wajib berisi tepat satu statement (kalau lebih,
#      ditolak sebelum apa pun dijalankan, karena kegagalan di tengah tidak
#      bisa di-rollback). Sesudahnya index INVALID (sisa build CONCURRENTLY
#      yang gagal) dilaporkan; catatan hanya ditulis bila semuanya valid.
#   8. Verifikasi seluruh "DB guard" (partial index, CHECK, trigger) lewat
#      db/verify-guards.sql, pengganti prisma/apply-partial-indexes.js; hanya
#      membaca katalog.
#
# Pemakaian (dari root project, atau path lengkap ke script ini):
#   db/migrate.sh --status                 # migration yang belum diterapkan
#   db/migrate.sh                          # migration terbaru
#   db/migrate.sh divisions                # cocokkan sebagian nama folder
#   db/migrate.sh --list                   # 10 migration terakhir (tanpa DB)
#   db/migrate.sh --no-backup              # DB lokal saja; ditolak di produksi
#   db/migrate.sh --verify-guards          # hanya periksa DB guard, tanpa migrasi
#   db/migrate.sh --baseline <nama>        # SEKALI per database: tandai semua
#                                          # migration s.d. <nama> sudah diterapkan
#
# Opsi:
#   --status              Bandingkan db/migrations dengan catatan di database.
#   --list                Tampilkan 10 migration terakhir lalu keluar.
#   --baseline <nama>     Catat migration s.d. <nama> sebagai sudah diterapkan
#                         TANPA menjalankannya (database lama yang dimigrasi
#                         manual sebelum ada catatan). Pilih <nama> = migration
#                         terakhir yang PASTI sudah ada di database itu.
#   --allow-destructive   Izinkan migration yang bisa menghapus/menimpa data.
#   --force               Jalankan ulang migration yang sudah tercatat.
#   --no-backup           Lewati pg_dump (hanya non-produksi).
#   --skip-post           Jangan verifikasi DB guard sesudah migration.
#   --verify-guards       Periksa DB guard (db/verify-guards.sql) lalu keluar.
#   --yes                 Lewati konfirmasi (hanya non-produksi).
#   -h, --help            Bantuan ini.
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

MIGRATIONS_DIR="${SCRIPT_DIR}/migrations"
GUARDS_SQL="${SCRIPT_DIR}/verify-guards.sql"
ENV_FILE="${ENV_FILE:-${PROJECT_ROOT}/.env}"
BACKUP_DIR="${BACKUP_DIR:-${PROJECT_ROOT}/backups/migrations}"
BACKUP_CONF="${BACKUP_CONF:-/etc/mudah-api-backup.conf}"

usage() { sed -n '2,/^set -Eeuo/p' "${BASH_SOURCE[0]}" | sed -e '$d' -e 's/^# \{0,1\}//'; }

PATTERN=""
MODE=apply        # apply | list | status | verify | baseline
NO_BACKUP=0
SKIP_POST=0
ASSUME_YES=0
ALLOW_DESTRUCTIVE=0
FORCE=0

set_mode() {
  if [[ "${MODE}" != apply && "${MODE}" != "$1" ]]; then
    err "--${MODE} dan --$1 tidak bisa dipakai bersamaan."
    exit 2
  fi
  MODE="$1"
}

while (( $# > 0 )); do
  case "$1" in
    --list)              set_mode list ;;
    --status)            set_mode status ;;
    --verify-guards)     set_mode verify ;;
    --baseline)          set_mode baseline ;;
    --allow-destructive) ALLOW_DESTRUCTIVE=1 ;;
    --force)             FORCE=1 ;;
    --no-backup)         NO_BACKUP=1 ;;
    --skip-post)         SKIP_POST=1 ;;
    --yes|-y)            ASSUME_YES=1 ;;
    -h|--help)           usage; exit 0 ;;
    -*)                  err "Opsi tidak dikenal: $1"; usage; exit 2 ;;
    *)
      if [[ -n "${PATTERN}" ]]; then
        err "Hanya satu nama migration yang boleh diberikan."
        exit 2
      fi
      PATTERN="$1"
      ;;
  esac
  shift
done

if [[ "${MODE}" == baseline && -z "${PATTERN}" ]]; then
  err "--baseline butuh nama migration terakhir yang sudah ada di database, mis. --baseline 20261119100000"
  exit 2
fi

# Nama folder migration hanya huruf, angka, dan garis bawah.
if [[ -n "${PATTERN}" && ! "${PATTERN}" =~ ^[A-Za-z0-9_]+$ ]]; then
  err "Nama migration hanya boleh huruf, angka, dan '_': ${PATTERN}"
  exit 2
fi

# Folder migration, urut naik (prefix timestamp = urutan kronologis).
mapfile -t ALL_MIGRATIONS < <(
  find "${MIGRATIONS_DIR}" -mindepth 2 -maxdepth 2 -name migration.sql -printf '%h\n' \
    | xargs -r -n1 basename | sort
)
if (( ${#ALL_MIGRATIONS[@]} == 0 )); then
  err "Tidak ada migration.sql di ${MIGRATIONS_DIR}."
  exit 1
fi
# Nama folder ikut ditulis sebagai literal SQL ke _db_migrations, jadi bentuknya dikunci.
for m in "${ALL_MIGRATIONS[@]}"; do
  if [[ ! "${m}" =~ ^[0-9]{14}_[A-Za-z0-9_]*$ ]]; then
    err "Nama folder migration tidak sah (harus <14 digit timestamp>_<nama>): ${m}"
    exit 1
  fi
done

if [[ "${MODE}" == list ]]; then
  info "10 migration terakhir (paling bawah = terbaru):"
  printf '  %s\n' "${ALL_MIGRATIONS[@]: -10}" >&2
  exit 0
fi

checksum_of() { sha256sum "${MIGRATIONS_DIR}/$1/migration.sql" | cut -d' ' -f1; }

# ---------------------------------------------------------------------------
# Pilih migration (apply / baseline)
# ---------------------------------------------------------------------------
MIGRATION=""
CONCURRENT=0
DESTRUCTIVE=0
if [[ "${MODE}" == apply || "${MODE}" == baseline ]]; then
  if [[ -z "${PATTERN}" ]]; then
    MIGRATION="${ALL_MIGRATIONS[-1]}"
  else
    mapfile -t MATCHES < <(printf '%s\n' "${ALL_MIGRATIONS[@]}" | grep -F -- "${PATTERN}" || true)
    if (( ${#MATCHES[@]} == 0 )); then
      err "Tidak ada migration yang namanya mengandung '${PATTERN}'. Lihat: db/migrate.sh --list"
      exit 1
    fi
    if (( ${#MATCHES[@]} > 1 )); then
      err "'${PATTERN}' cocok dengan ${#MATCHES[@]} migration; perjelas namanya:"
      printf '  %s\n' "${MATCHES[@]}" >&2
      exit 1
    fi
    MIGRATION="${MATCHES[0]}"
  fi
fi

if [[ "${MODE}" == apply ]]; then
  SQL_FILE="${MIGRATIONS_DIR}/${MIGRATION}/migration.sql"

  # Isi SQL tanpa komentar baris (`-- ...`), supaya kata di komentar (mis.
  # penjelasan "CONCURRENTLY tidak boleh di dalam transaksi") tidak ikut terdeteksi.
  sql_without_comments() { sed -E 's/--.*$//' "${SQL_FILE}"; }

  if sql_without_comments | grep -qiE '\bCONCURRENTLY\b'; then
    CONCURRENT=1
    STATEMENTS="$(sql_without_comments | tr -cd ';' | wc -c)"
    if (( STATEMENTS != 1 )); then
      err "${MIGRATION} berisi CONCURRENTLY, jadi harus dijalankan di luar transaksi,"
      err "tetapi file ini punya ${STATEMENTS} statement (dihitung dari ';'). Tanpa transaksi,"
      err "kegagalan di tengah meninggalkan sebagian perubahan. Pecah jadi satu statement per folder."
      exit 1
    fi
  fi

  # -------------------------------------------------------------------------
  # Blokir perintah yang bisa menghapus/menimpa data. Dicocokkan pada SQL
  # tanpa komentar yang dijadikan satu baris huruf besar, supaya perintah yang
  # dipecah ke beberapa baris (`DROP\n  COLUMN`) tetap tertangkap. Salah
  # tangkap (mis. UPDATE di dalam badan fungsi trigger) sengaja dibiarkan:
  # akibatnya hanya perlu --allow-destructive, bukan data yang hilang.
  # -------------------------------------------------------------------------
  SQL_FLAT="$(sql_without_comments | tr '\n\r\t' '   ' | tr -s ' ' | tr '[:lower:]' '[:upper:]')"
  SQL_FLAT_NO_FK="$(sed -E 's/ON (DELETE|UPDATE) CASCADE//g' <<<"${SQL_FLAT}")"
  REASONS=()
  flag() { grep -qE "$2" <<<"${3:-${SQL_FLAT}}" && REASONS+=("$1") || true; }
  flag "DROP TABLE"                       '\bDROP TABLE\b'
  flag "DROP COLUMN"                      '\bDROP COLUMN\b'
  flag "DROP SCHEMA/TYPE/SEQUENCE/DATABASE" '\bDROP (SCHEMA|TYPE|SEQUENCE|DATABASE)\b'
  flag "TRUNCATE"                         '\bTRUNCATE\b'
  flag "DELETE FROM"                      '\bDELETE FROM\b'
  flag "UPDATE … SET (menimpa data)"      '\bUPDATE( [^ ;]+){0,3} SET\b'
  flag "ALTER COLUMN … TYPE (konversi)"   '\bALTER COLUMN [^ ;]+ (SET DATA )?TYPE\b'
  flag "CASCADE (ikut menghapus objek terkait)" '\bCASCADE\b' "${SQL_FLAT_NO_FK}"

  if (( ${#REASONS[@]} > 0 )); then
    DESTRUCTIVE=1
    report="$(printf '    - %s\n' "${REASONS[@]}")"
    # Baris yang memuat kata kunci di atas; aksi FK (`ON DELETE/UPDATE CASCADE|RESTRICT…`)
    # dan event trigger (`BEFORE UPDATE ON`, `UPDATE OF`) diabaikan supaya tidak
    # menenggelamkan baris yang sebenarnya.
    lines="$(sed -E 's/--.*$//' "${SQL_FILE}" | awk '{
      t = toupper($0)
      gsub(/ON (DELETE|UPDATE) (CASCADE|RESTRICT|NO ACTION|SET NULL|SET DEFAULT)/, "", t)
      gsub(/(BEFORE|AFTER|OR) (DELETE|UPDATE)|UPDATE OF/, "", t)
      if (t ~ /(^|[^A-Z_])(DROP|TRUNCATE|DELETE|UPDATE|CASCADE)([^A-Z_]|$)|ALTER[ \t]+COLUMN/ && n < 20) {
        n++; printf "    %d:%s\n", NR, $0
      }
    }')" || true
    if (( ! ALLOW_DESTRUCTIVE )); then
      err "${MIGRATION} berisi perintah yang bisa MENGHAPUS atau MENIMPA data:"
      printf '%s\n' "${report}" >&2
      err "Baris terkait (${SQL_FILE}):"
      printf '%s\n' "${lines}" >&2
      err "Migration TIDAK dijalankan. Kalau memang disengaja, baca file-nya lalu ulangi dengan --allow-destructive."
      exit 1
    fi
    warn "${MIGRATION} bisa MENGHAPUS atau MENIMPA data (diizinkan --allow-destructive):"
    printf '%s\n' "${report}" >&2
    printf '%s\n' "${lines}" >&2
  fi
fi

# ---------------------------------------------------------------------------
# Koneksi
# ---------------------------------------------------------------------------
load_backup_conf
resolve_direct_database_url
export_pg_env
reject_do_pool_port || exit 1

IS_PROD=0
if [[ "${PGHOST}" == *.db.ondigitalocean.com || "${NODE_ENV:-}" == "production" ]]; then
  IS_PROD=1
fi

require_pg_clients psql
if ! psql -X -tAc 'SELECT 1' >/dev/null 2>&1; then
  err "Tidak bisa connect ke ${PGUSER}@${PGHOST}:${PGPORT}/${PGDATABASE}."
  err "Cek ${URL_SOURCE} (environment / ${BACKUP_CONF} / ${ENV_FILE}), Trusted Sources di panel, dan apakah database-nya sedang jalan."
  exit 1
fi
PROD_TAG=""
if (( IS_PROD )); then PROD_TAG='  [PRODUKSI]'; fi
DB_LABEL="${PGUSER}@${PGHOST}:${PGPORT}/${PGDATABASE}${PROD_TAG}  (dari ${URL_SOURCE})"

# Periksa DB guard (db/verify-guards.sql). 0 = lengkap, 1 = ada MISSING/OBSOLETE,
# 2 = query gagal. PENDING (tabel/kolom belum ada) hanya diperingatkan.
verify_guards() {
  local out status kind name reason bad=0
  if ! out="$(psql -X -tA -F '|' -v ON_ERROR_STOP=1 -f "${GUARDS_SQL}")"; then
    err "Verifikasi DB guard gagal dijalankan (${GUARDS_SQL})."
    return 2
  fi
  while IFS='|' read -r status kind name reason; do
    [[ -z "${status}" ]] && continue
    if [[ "${status}" == "PENDING" ]]; then
      warn "DB guard ${kind} ${name}: ${reason}"
    else
      err "DB guard ${status} ${kind} ${name}: ${reason}"
      bad=1
    fi
  done <<<"${out}"
  (( bad )) && return 1
  info "DB guard: semua terpasang."
  return 0
}

if [[ "${MODE}" == verify ]]; then
  info "Database  : ${DB_LABEL}"
  verify_guards
  exit $?
fi

# ---------------------------------------------------------------------------
# Catatan migrasi (_db_migrations)
# ---------------------------------------------------------------------------
TRACK_DDL='CREATE TABLE IF NOT EXISTS "_db_migrations" (
  "name"      text PRIMARY KEY,
  "checksum"  text NOT NULL,
  "appliedAt" timestamptz NOT NULL DEFAULT now(),
  "appliedBy" text NOT NULL DEFAULT current_user,
  "mode"      text NOT NULL CHECK ("mode" IN ('"'"'migrate'"'"', '"'"'force'"'"', '"'"'baseline'"'"'))
)'

# Baris INSERT untuk satu migration. Nama sudah divalidasi (^[0-9]{14}_[A-Za-z0-9_]*$),
# checksum = hex sha256, mode = kata tetap; aman sebagai literal.
record_sql() {
  local name="$1" mode="$2" on_conflict="$3"
  printf 'INSERT INTO "_db_migrations" ("name", "checksum", "mode") VALUES (%s, %s, %s) %s' \
    "'${name}'" "'$(checksum_of "${name}")'" "'${mode}'" "${on_conflict}"
}
UPSERT='ON CONFLICT ("name") DO UPDATE SET "checksum" = EXCLUDED."checksum", "appliedAt" = now(), "appliedBy" = current_user, "mode" = EXCLUDED."mode"'

declare -A APPLIED_AT=() APPLIED_SUM=() APPLIED_DESC=()
TRACKED=0
if [[ "$(psql -X -tAc "SELECT to_regclass('\"_db_migrations\"') IS NOT NULL")" == t ]]; then
  TRACKED=1
  while IFS='|' read -r name sum at by mode; do
    [[ -z "${name}" ]] && continue
    APPLIED_AT["${name}"]="${at}"
    APPLIED_SUM["${name}"]="${sum}"
    APPLIED_DESC["${name}"]="${at} oleh ${by} (${mode})"
  done < <(psql -X -tA -F '|' -v ON_ERROR_STOP=1 -c \
    'SELECT "name", "checksum", to_char("appliedAt", '"'"'YYYY-MM-DD HH24:MI:SS TZ'"'"'), "appliedBy", "mode" FROM "_db_migrations"')
fi

pending_before() {  # migration lebih lama dari $1 yang belum tercatat
  local m
  for m in "${ALL_MIGRATIONS[@]}"; do
    [[ "${m}" < "$1" ]] || break
    [[ -n "${APPLIED_AT[${m}]:-}" ]] || printf '%s\n' "${m}"
  done
}

if [[ "${MODE}" == status ]]; then
  info "Database  : ${DB_LABEL}"
  if (( ! TRACKED )); then
    warn "Belum ada catatan migrasi (_db_migrations) di database ini."
    warn "Tandai dulu migration yang sudah ada: db/migrate.sh --baseline <migration terakhir yang sudah diterapkan>"
    exit 1
  fi
  pending=() changed=()
  for m in "${ALL_MIGRATIONS[@]}"; do
    if [[ -z "${APPLIED_AT[${m}]:-}" ]]; then
      pending+=("${m}")
    elif [[ "${APPLIED_SUM[${m}]}" != "$(checksum_of "${m}")" ]]; then
      changed+=("${m}")
    fi
  done
  info "${#ALL_MIGRATIONS[@]} migration di repo, $(( ${#ALL_MIGRATIONS[@]} - ${#pending[@]} )) tercatat sudah diterapkan."
  if (( ${#pending[@]} > 0 )); then
    warn "${#pending[@]} BELUM diterapkan (urut):"
    printf '  %s\n' "${pending[@]}" >&2
  else
    info "Tidak ada migration yang tertunda."
  fi
  if (( ${#changed[@]} > 0 )); then
    warn "${#changed[@]} file migration BERUBAH sesudah diterapkan (migration yang sudah dirilis tidak boleh diubah):"
    printf '  %s\n' "${changed[@]}" >&2
  fi
  for name in "${!APPLIED_AT[@]}"; do
    [[ -d "${MIGRATIONS_DIR}/${name}" ]] || warn "Tercatat di database tapi tidak ada di repo: ${name}"
  done
  exit 0
fi

confirm() {  # konfirmasi umum: produksi = ketik nama database
  local answer
  if (( IS_PROD )); then
    printf 'Ketik nama database (%s) untuk melanjutkan: ' "${PGDATABASE}" >&2
    read -r answer
    [[ "${answer}" == "${PGDATABASE}" ]] || { err "Dibatalkan."; exit 1; }
  elif (( ! ASSUME_YES )); then
    printf 'Lanjutkan? [y/N] ' >&2
    read -r answer
    [[ "${answer}" =~ ^[yY]$ ]] || { err "Dibatalkan."; exit 1; }
  fi
}

if [[ "${MODE}" == baseline ]]; then
  to_mark=()
  for m in "${ALL_MIGRATIONS[@]}"; do
    [[ "${m}" > "${MIGRATION}" ]] && break
    [[ -n "${APPLIED_AT[${m}]:-}" ]] || to_mark+=("${m}")
  done
  info "Database  : ${DB_LABEL}"
  info "Baseline  : tandai ${#to_mark[@]} migration s.d. ${MIGRATION} sebagai SUDAH diterapkan, TANPA menjalankannya."
  warn "Pastikan semuanya memang sudah ada di database ini. Migration yang ditandai tidak akan bisa dijalankan tanpa --force."
  if (( ${#to_mark[@]} == 0 )); then
    info "Tidak ada yang perlu ditandai."
    exit 0
  fi
  confirm
  # BEGIN/COMMIT eksplisit: --single-transaction tidak berlaku untuk input stdin.
  # ON_ERROR_STOP memutus koneksi sebelum COMMIT bila ada error -> rollback.
  sql="BEGIN;"$'\n'"${TRACK_DDL};"
  for m in "${to_mark[@]}"; do
    sql+=$'\n'"$(record_sql "${m}" baseline 'ON CONFLICT ("name") DO NOTHING');"
  done
  sql+=$'\n'"COMMIT;"
  psql -X -v ON_ERROR_STOP=1 -q <<<"${sql}"
  info "Baseline selesai: ${#to_mark[@]} migration ditandai. Cek: db/migrate.sh --status"
  exit 0
fi

# ---------------------------------------------------------------------------
# MODE apply: pemeriksaan catatan sebelum konfirmasi
# ---------------------------------------------------------------------------
info "Migration : ${MIGRATION}$( (( CONCURRENT )) && printf '  [CONCURRENTLY: tanpa --single-transaction]')"
info "Database  : ${DB_LABEL}"

if (( ! TRACKED )); then
  err "Database ini belum punya catatan migrasi (_db_migrations), jadi tidak bisa dipastikan"
  err "${MIGRATION} belum pernah diterapkan. Tandai dulu migration yang sudah ada (sekali saja):"
  err "  db/migrate.sh --baseline <migration terakhir yang sudah diterapkan>"
  exit 1
fi

if [[ -n "${APPLIED_AT[${MIGRATION}]:-}" ]]; then
  if (( ! FORCE )); then
    err "${MIGRATION} SUDAH diterapkan: ${APPLIED_DESC[${MIGRATION}]}."
    err "Menjalankan ulang bisa mengubah data lagi tanpa error. Lihat: db/migrate.sh --status"
    err "Kalau memang sengaja dijalankan ulang, tambahkan --force."
    exit 1
  fi
  warn "${MIGRATION} sudah diterapkan (${APPLIED_DESC[${MIGRATION}]}); dijalankan ULANG karena --force."
  [[ "${APPLIED_SUM[${MIGRATION}]}" == "$(checksum_of "${MIGRATION}")" ]] \
    || warn "Isi file berubah sejak diterapkan."
fi

mapfile -t EARLIER < <(pending_before "${MIGRATION}")
if (( ${#EARLIER[@]} > 0 )); then
  warn "${#EARLIER[@]} migration LEBIH LAMA belum tercatat diterapkan (urutan tidak dipaksa, tapi periksa):"
  printf '  %s\n' "${EARLIER[@]: 0:10}" >&2
  if (( ${#EARLIER[@]} > 10 )); then
    printf '  … dan %d lagi\n' $(( ${#EARLIER[@]} - 10 )) >&2
  fi
fi

if (( IS_PROD && NO_BACKUP )); then
  err "--no-backup ditolak untuk database produksi."
  exit 1
fi
confirm
if (( IS_PROD && DESTRUCTIVE )); then
  printf 'Migration ini bisa MENGHAPUS/MENIMPA data produksi. Ketik HAPUS untuk melanjutkan: ' >&2
  read -r answer
  [[ "${answer}" == "HAPUS" ]] || { err "Dibatalkan."; exit 1; }
fi
if (( IS_PROD && FORCE )) && [[ -n "${APPLIED_AT[${MIGRATION}]:-}" ]]; then
  printf 'Menjalankan ULANG migration di produksi. Ketik nama migration (%s): ' "${MIGRATION}" >&2
  read -r answer
  [[ "${answer}" == "${MIGRATION}" ]] || { err "Dibatalkan."; exit 1; }
fi

# ---------------------------------------------------------------------------
# Backup
# ---------------------------------------------------------------------------
DUMP_TMP=""
on_exit() {
  if [[ -n "${DUMP_TMP}" && -f "${DUMP_TMP}" ]]; then
    rm -f "${DUMP_TMP}"
  fi
}
trap on_exit EXIT
trap 'err "Dihentikan."; exit 143' INT TERM

if (( NO_BACKUP )); then
  warn "Backup dilewati (--no-backup)."
else
  require_pg_clients pg_dump pg_restore
  if ! check_server_version pg_dump; then
    err "Backup tidak bisa dibuat, jadi migration TIDAK dijalankan."
    (( IS_PROD )) || err "Untuk DB lokal, bisa juga: db/migrate.sh --no-backup"
    exit 1
  fi
  mkdir -p "${BACKUP_DIR}"
  chmod 700 "${BACKUP_DIR}" 2>/dev/null || true
  DUMP_PATH="${BACKUP_DIR}/${PGDATABASE}-before-${MIGRATION}-$(date +%Y%m%d-%H%M%S).dump"
  DUMP_TMP="${DUMP_PATH}.part"
  info "Backup -> ${DUMP_PATH}"
  pg_dump --format=custom --no-owner --no-privileges --file="${DUMP_TMP}"
  if ! pg_restore --list "${DUMP_TMP}" >/dev/null 2>&1; then
    err "Backup tidak lolos verifikasi pg_restore --list. Migration TIDAK dijalankan."
    exit 1
  fi
  mv -f "${DUMP_TMP}" "${DUMP_PATH}"
  chmod 600 "${DUMP_PATH}"
  DUMP_TMP=""
  info "Backup OK ($(du -h "${DUMP_PATH}" | cut -f1))."
fi

# ---------------------------------------------------------------------------
# Terapkan migration
# ---------------------------------------------------------------------------
RECORD_MODE=migrate
if (( FORCE )) && [[ -n "${APPLIED_AT[${MIGRATION}]:-}" ]]; then
  RECORD_MODE=force
fi
RECORD="$(record_sql "${MIGRATION}" "${RECORD_MODE}" "${UPSERT}")"

# Index yang ditinggal build CONCURRENTLY gagal: tetap ada tapi tidak dipakai
# planner, dan `CREATE INDEX ... IF NOT EXISTS` berikutnya melewatinya diam-diam.
report_invalid_indexes() {
  local invalid
  invalid="$(psql -X -tA -c "SELECT n.nspname || '.' || c.relname FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE NOT i.indisvalid ORDER BY 1")" || {
    warn "Tidak bisa memeriksa index INVALID (pg_index)."
    return 0
  }
  [[ -z "${invalid}" ]] && return 0
  warn "Ada index INVALID (build CONCURRENTLY yang gagal). Hapus dengan DROP INDEX CONCURRENTLY, lalu jalankan ulang migration:"
  sed 's/^/  /' <<<"${invalid}" >&2
  return 1
}

if (( CONCURRENT )); then
  info "Menjalankan ${MIGRATION}/migration.sql di luar transaksi (CONCURRENTLY)..."
  if ! psql -X -v ON_ERROR_STOP=1 -f "${SQL_FILE}"; then
    err "Migration GAGAL. Satu-satunya statement tidak selesai, tetapi build CONCURRENTLY"
    err "yang gagal bisa meninggalkan index INVALID:"
    report_invalid_indexes || true
    exit 1
  fi
  if ! report_invalid_indexes; then
    err "Migration selesai tanpa error, tetapi index di atas INVALID (IF NOT EXISTS melewati index INVALID yang sudah ada)."
    err "Catatan _db_migrations TIDAK ditulis."
    exit 1
  fi
  psql -X -q -v ON_ERROR_STOP=1 -c "${RECORD}"
else
  info "Menjalankan ${MIGRATION}/migration.sql dalam satu transaksi (beserta catatannya)..."
  if ! psql -X --single-transaction -v ON_ERROR_STOP=1 -f "${SQL_FILE}" -c "${RECORD}"; then
    err "Migration GAGAL dan seluruhnya di-rollback; database tidak berubah."
    if [[ "${RECORD_MODE}" == force ]]; then
      err "Ini menjalankan ULANG migration yang sudah tercatat; error 'already exists' berarti isinya memang sudah ada."
    else
      err "Kalau errornya 'already exists', migration ini kemungkinan sudah pernah diterapkan"
      err "tanpa tercatat: periksa, lalu tandai dengan --baseline bila memang begitu."
    fi
    exit 1
  fi
fi
info "Migration berhasil diterapkan dan dicatat di _db_migrations."

# ---------------------------------------------------------------------------
# Langkah sesudahnya
# ---------------------------------------------------------------------------
if (( SKIP_POST )); then
  warn "Verifikasi DB guard dilewati (--skip-post). Jalankan sendiri: db/migrate.sh --verify-guards"
else
  # Selalu, bukan hanya bila migration-nya tampak berisi guard: pola grep per baris
  # (versi Node) melewatkan `CREATE UNIQUE INDEX` yang `WHERE`-nya di baris lain.
  # Verifikasinya hanya membaca katalog, < 1 detik.
  info "Memverifikasi DB guard (db/verify-guards.sql)..."
  if ! verify_guards; then
    err "Migration SUDAH diterapkan, tetapi ada DB guard yang hilang/tidak sesuai (lihat di atas)."
    err "Kalau migration ini menambah/mengubah guard, perbarui barisnya di db/verify-guards.sql."
    exit 1
  fi
fi

info "Selesai. Kalau rilis ini menambah key fitur di feature_registry.rs, isi juga izin role bawaan (rolePermission)."
(( NO_BACKUP )) || info "Kalau perlu kembali: db/restore-db.sh ${DUMP_PATH}  (default: ke database baru)"
