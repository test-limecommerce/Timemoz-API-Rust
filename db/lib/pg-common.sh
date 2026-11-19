#!/usr/bin/env bash
#
# pg-common.sh — fungsi bersama untuk backup-db.sh, restore-db.sh, dan migrate.sh.
# File ini di-SOURCE, bukan dieksekusi langsung.
#
# Menyediakan: logging, pemuatan config, parsing DATABASE_URL menjadi variabel
# libpq, pengecekan client, dan preflight versi server.
#

# Variabel yang boleh di-override lewat environment (mis. dari baris cron).
# Nilai environment harus MENANG atas file config, jadi disimpan sebelum config
# di-source lalu dipasang kembali sesudahnya.
PG_COMMON_OVERRIDABLE=(
  DATABASE_URL MIGRATION_DATABASE_URL BACKUP_DIR RETENTION_DAYS LOG_FILE ENV_FILE PGSSLROOTCERT
  B2_KEY_ID B2_APP_KEY B2_BUCKET_ID B2_PREFIX
)

# ---------------------------------------------------------------------------
# Logging — aman dipakai sebelum LOG_FILE ditentukan (jatuh ke stderr saja).
# ---------------------------------------------------------------------------
log() {
  local line
  line="$(date '+%Y-%m-%d %H:%M:%S') [${1}] ${2}"
  if [[ -n "${LOG_FILE:-}" && -w "${LOG_FILE}" ]]; then
    printf '%s\n' "${line}" | tee -a "${LOG_FILE}" >&2
  else
    printf '%s\n' "${line}" >&2
  fi
}
info() { log "INFO" "$*"; }
warn() { log "WARN" "$*"; }
err()  { log "ERROR" "$*"; }

# ---------------------------------------------------------------------------
# Config file di luar repo (server tidak punya .env — lihat komentar di
# backup-db.sh). Menetapkan CONF_LOADED=1 kalau berhasil dimuat.
# ---------------------------------------------------------------------------
load_backup_conf() {
  local v
  for v in "${PG_COMMON_OVERRIDABLE[@]}"; do
    eval "_env_${v}=\"\${${v}:-}\""
  done

  if [[ -r "${BACKUP_CONF}" ]]; then
    # shellcheck source=/dev/null
    source "${BACKUP_CONF}"
    CONF_LOADED=1
  fi

  for v in "${PG_COMMON_OVERRIDABLE[@]}"; do
    eval "[[ -n \"\${_env_${v}}\" ]] && ${v}=\"\${_env_${v}}\"" || true
  done
  return 0
}

# ---------------------------------------------------------------------------
# Nilai KEY di file .env: baris terakhir yang tidak dikomentari; quote & CR
# dibuang. Kosong (exit 0) kalau file tak terbaca atau KEY tidak ada.
# $1 = KEY (huruf besar, angka, garis bawah), $2 = path file.
# ---------------------------------------------------------------------------
env_file_value() {
  local key="$1" file="$2"
  [[ "${key}" =~ ^[A-Z_][A-Z0-9_]*$ ]] || { err "Nama variabel tidak valid: ${key}"; return 1; }
  [[ -r "${file}" ]] || return 0
  { grep -E "^[[:space:]]*${key}[[:space:]]*=" "${file}" || true; } \
    | tail -n 1 \
    | sed -E "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*//; s/\r\$//; s/^\"(.*)\"\$/\1/; s/^'(.*)'\$/\1/"
}

# ---------------------------------------------------------------------------
# DATABASE_URL: environment > file config > .env di root project
# ---------------------------------------------------------------------------
resolve_database_url() {
  if [[ -z "${DATABASE_URL:-}" ]]; then
    if [[ ! -r "${ENV_FILE}" ]]; then
      err "DATABASE_URL tidak ada di environment maupun ${BACKUP_CONF},"
      err "dan ${ENV_FILE} tidak bisa dibaca."
      return 1
    fi
    DATABASE_URL="$(env_file_value DATABASE_URL "${ENV_FILE}")"
  fi

  if [[ -z "${DATABASE_URL}" ]]; then
    err "DATABASE_URL kosong."
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------
# URL untuk pg_dump / DDL: MIGRATION_DATABASE_URL (koneksi LANGSUNG, tanpa
# pool) kalau ada, selain itu DATABASE_URL. Masing-masing: environment > file
# config > .env. Menetapkan URL_SOURCE = nama variabel yang terpakai.
# ---------------------------------------------------------------------------
resolve_direct_database_url() {
  if [[ -z "${MIGRATION_DATABASE_URL:-}" ]]; then
    MIGRATION_DATABASE_URL="$(env_file_value MIGRATION_DATABASE_URL "${ENV_FILE}")"
  fi
  if [[ -n "${MIGRATION_DATABASE_URL}" ]]; then
    DATABASE_URL="${MIGRATION_DATABASE_URL}"
    URL_SOURCE="MIGRATION_DATABASE_URL"
  else
    resolve_database_url || return 1
    URL_SOURCE="DATABASE_URL"
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Tolak connection pool DigitalOcean (port 25061, PgBouncer mode transaksi).
# pg_dump mengosongkan search_path; PgBouncer di depan PostgreSQL 18 memutus
# koneksinya ("FATAL: invalid server parameter"), jadi dump pasti gagal.
# Dipanggil SESUDAH export_pg_env.
# ---------------------------------------------------------------------------
reject_do_pool_port() {
  if [[ "${PGHOST}" == *.db.ondigitalocean.com && "${PGPORT}" == "25061" ]]; then
    err "${URL_SOURCE:-DATABASE_URL} menunjuk ke connection pool DigitalOcean (port 25061)."
    err "pg_dump tidak bisa lewat pool. Set MIGRATION_DATABASE_URL ke koneksi langsung:"
    err "  port 25060 + nama database asli (panel DO > Connection Details > pilih database, bukan pool)."
    err "Simpan MIGRATION_DATABASE_URL=\"...\" di ${BACKUP_CONF}, atau export di environment."
    return 1
  fi
  return 0
}

urldecode() { printf '%b' "${1//%/\\x}"; }

# ---------------------------------------------------------------------------
# Parse DATABASE_URL -> variabel libpq (PGHOST, PGPORT, PGUSER, ...).
# Query string Prisma (?schema=public) dibuang karena bukan parameter libpq;
# hanya sslmode dan sslrootcert yang diteruskan.
#
# Menetapkan juga SRC_DB_NAME (nama database asal) untuk dipakai pemanggil.
# ---------------------------------------------------------------------------
export_pg_env() {
  local proto_stripped query_string userinfo hostpart hostport
  local db_user db_pass db_name db_host db_port db_sslmode db_sslrootcert
  local p params

  proto_stripped="${DATABASE_URL#*://}"
  query_string=""
  if [[ "${proto_stripped}" == *"?"* ]]; then
    query_string="${proto_stripped#*\?}"
    proto_stripped="${proto_stripped%%\?*}"
  fi

  userinfo=""
  hostpart="${proto_stripped}"
  if [[ "${proto_stripped}" == *"@"* ]]; then
    userinfo="${proto_stripped%@*}"     # sampai '@' TERAKHIR, jadi password
    hostpart="${proto_stripped##*@}"    # boleh mengandung '@'
  fi

  db_user="${userinfo%%:*}"
  db_pass=""
  [[ "${userinfo}" == *":"* ]] && db_pass="${userinfo#*:}"

  if [[ "${hostpart}" != */* ]]; then
    err "DATABASE_URL tidak menyertakan nama database (contoh: .../timemoz?sslmode=require)."
    return 1
  fi
  hostport="${hostpart%%/*}"
  db_name="${hostpart#*/}"
  db_host="${hostport%%:*}"
  db_port="5432"
  [[ "${hostport}" == *":"* ]] && db_port="${hostport##*:}"

  db_sslmode=""
  db_sslrootcert=""
  if [[ -n "${query_string}" ]]; then
    IFS='&' read -r -a params <<< "${query_string}"
    for p in "${params[@]}"; do
      case "${p%%=*}" in
        sslmode)     db_sslmode="${p#*=}" ;;
        sslrootcert) db_sslrootcert="$(urldecode "${p#*=}")" ;;
      esac
    done
  fi

  if [[ -z "${db_host}" || -z "${db_name}" || -z "${db_user}" ]]; then
    err "DATABASE_URL tidak valid — host/database/user tidak terbaca."
    return 1
  fi

  export PGHOST PGPORT PGUSER PGDATABASE
  PGHOST="$(urldecode "${db_host}")"
  PGPORT="${db_port}"
  PGUSER="$(urldecode "${db_user}")"
  PGDATABASE="$(urldecode "${db_name}")"
  SRC_DB_NAME="${PGDATABASE}"

  [[ -n "${db_pass}"    ]] && export PGPASSWORD="$(urldecode "${db_pass}")"
  [[ -n "${db_sslmode}" ]] && export PGSSLMODE="${db_sslmode}"

  # CA certificate — hanya perlu untuk sslmode=verify-ca / verify-full.
  # DO Managed Database: unduh ca-certificate.crt dari panel, lalu set
  # PGSSLROOTCERT di file config.
  if [[ -n "${PGSSLROOTCERT:-}" ]]; then
    [[ -r "${PGSSLROOTCERT}" ]] || { err "PGSSLROOTCERT tidak terbaca: ${PGSSLROOTCERT}"; return 1; }
    export PGSSLROOTCERT
  elif [[ -n "${db_sslrootcert}" ]]; then
    [[ -r "${db_sslrootcert}" ]] || { err "sslrootcert dari DATABASE_URL tidak terbaca: ${db_sslrootcert}"; return 1; }
    export PGSSLROOTCERT="${db_sslrootcert}"
  fi
  export PGCONNECT_TIMEOUT="${PGCONNECT_TIMEOUT:-15}"
  return 0
}

# ---------------------------------------------------------------------------
# Instruksi pasang postgresql-client dari repo resmi PostgreSQL (PGDG).
# Repo bawaan Ubuntu tertinggal versi dibanding managed database.
# ---------------------------------------------------------------------------
print_install_hint() {
  # Default 18 — versi DO Managed Database project ini.
  # Client yang lebih baru dari server selalu aman; yang lebih tua ditolak.
  local major="${1:-18}"
  err "Pasang postgresql-client dari repo PGDG:"
  err "  sudo apt install -y curl ca-certificates gnupg lsb-release"
  err "  curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc | sudo gpg --dearmor -o /usr/share/keyrings/pgdg.gpg"
  err "  echo \"deb [signed-by=/usr/share/keyrings/pgdg.gpg] https://apt.postgresql.org/pub/repos/apt \$(lsb_release -cs)-pgdg main\" | sudo tee /etc/apt/sources.list.d/pgdg.list"
  err "  sudo apt update && sudo apt install -y postgresql-client-${major}"
}

require_pg_clients() {
  local b
  for b in "$@"; do
    if ! command -v "${b}" >/dev/null; then
      err "${b} tidak ditemukan di PATH."
      print_install_hint
      return 1
    fi
  done
  return 0
}

# ---------------------------------------------------------------------------
# Preflight: koneksi + kecocokan versi.
# pg_dump/pg_restore MENOLAK arsip dari server yang versi mayornya lebih baru.
# Menetapkan SERVER_MAJOR. Argumen $1 = nama binari client yang dicek versinya.
# ---------------------------------------------------------------------------
check_server_version() {
  local client_bin="${1:-pg_dump}" client_major server_version_num
  client_major="$("${client_bin}" --version | sed -nE 's/.* ([0-9]+)(\.[0-9]+)?.*/\1/p')"

  if ! server_version_num="$(psql -tAX -c 'SHOW server_version_num' 2>&1)"; then
    err "Tidak bisa connect ke database: ${server_version_num}"
    err "Cek kredensial, sslmode, dan Trusted Sources di panel database."
    return 1
  fi

  SERVER_MAJOR=$(( server_version_num / 10000 ))
  info "Server PostgreSQL ${SERVER_MAJOR} — ${client_bin} ${client_major}"

  if (( client_major < SERVER_MAJOR )); then
    err "${client_bin} ${client_major} terlalu tua untuk server PostgreSQL ${SERVER_MAJOR}."
    print_install_hint "${SERVER_MAJOR}"
    return 1
  fi
  return 0
}
