#!/usr/bin/env bash
#
# sync-from-node.sh — MASA TRANSISI SAJA: salin migrasi baru dari repo Mudah-API.
#
# Selama perubahan skema masih ditulis di repo Node (prisma/migrations), tiap port
# commit Node yang membawa migration juga harus membawa folder migration-nya ke
# db/migrations. Script ini melakukannya secara mekanis:
#   - folder migration yang belum ada di sini  -> disalin
#   - folder yang ADA di keduanya tapi isinya beda -> dilaporkan, TIDAK ditimpa
#     (migration yang sudah dirilis tidak boleh berubah; selidiki dulu)
#   - folder yang hanya ada di sini            -> dilaporkan (repo Node tertinggal,
#     mis. branch lama); schema.prisma & verify-guards.sql lalu TIDAK ditimpa
#   - prisma/schema.prisma                      -> disalin ke db/schema.prisma bila beda
#   - db/verify-guards.sql dibuat ulang dari prisma/apply-partial-indexes.js
#     (butuh `node`; dilewati dengan peringatan bila tidak ada)
# Tidak menyentuh database. Hasilnya diperiksa dengan `git diff db/` lalu di-commit.
#
# Sesudah cutover (skema dimiliki repo ini), script ini tidak dipakai lagi: tulis
# migration baru langsung di db/migrations/<timestamp>_<nama>/migration.sql.
#
# Pemakaian:
#   db/sync-from-node.sh ../Mudah-API
#
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
NODE_REPO="${1:-}"
[[ -n "${NODE_REPO}" ]] || { echo "Pakai: $0 <path repo Mudah-API>" >&2; exit 2; }
SRC="${NODE_REPO}/prisma/migrations"
DST="${SCRIPT_DIR}/migrations"
[[ -d "${SRC}" ]] || { echo "Tidak ada ${SRC}" >&2; exit 1; }

label="$(git -C "${NODE_REPO}" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?') $(git -C "${NODE_REPO}" rev-parse --short HEAD 2>/dev/null || echo '?')"
echo "Sumber: ${NODE_REPO} (${label})"
if [[ -n "$(git -C "${NODE_REPO}" status --porcelain -- prisma/migrations prisma/schema.prisma prisma/apply-partial-indexes.js 2>/dev/null)" ]]; then
  echo "PERINGATAN: migrasi/skema/guard di repo Node punya perubahan yang belum di-commit." >&2
fi

added=0 changed=0 behind=0
for dir in "${SRC}"/*/; do
  name="$(basename "${dir}")"
  if [[ ! -d "${DST}/${name}" ]]; then
    cp -r "${dir%/}" "${DST}/${name}"
    echo "  + ${name}"
    added=$((added + 1))
  elif ! diff -rq "${dir%/}" "${DST}/${name}" >/dev/null; then
    echo "  ! ${name}: isinya BEDA dengan versi di sini (tidak ditimpa)" >&2
    changed=$((changed + 1))
  fi
done
for dir in "${DST}"/*/; do
  name="$(basename "${dir}")"
  if [[ ! -d "${SRC}/${name}" ]]; then
    echo "  ? ${name}: hanya ada di repo Rust"
    behind=$((behind + 1))
  fi
done
echo "Migration baru: ${added}; beda isi: ${changed}"

if (( behind > 0 )); then
  echo "PERINGATAN: repo Node tertinggal (${behind} migration hanya ada di sini); db/schema.prisma dan" >&2
  echo "db/verify-guards.sql TIDAK diperbarui dari branch ini. Sinkronkan dari branch yang memuat semuanya." >&2
elif ! cmp -s "${NODE_REPO}/prisma/schema.prisma" "${SCRIPT_DIR}/schema.prisma"; then
  cp "${NODE_REPO}/prisma/schema.prisma" "${SCRIPT_DIR}/schema.prisma"
  echo "db/schema.prisma diperbarui."
fi

if (( behind > 0 )); then
  :
elif command -v node >/dev/null && [[ -f "${NODE_REPO}/prisma/apply-partial-indexes.js" ]]; then
  before="$(cksum < "${SCRIPT_DIR}/verify-guards.sql")"
  node "${SCRIPT_DIR}/sync/gen-verify-guards.cjs" "${NODE_REPO}" "${SCRIPT_DIR}/verify-guards.sql" "${label}" >/dev/null
  [[ "${before}" == "$(cksum < "${SCRIPT_DIR}/verify-guards.sql")" ]] || echo "db/verify-guards.sql diperbarui."
else
  echo "PERINGATAN: node tidak ada; db/verify-guards.sql tidak dibuat ulang." >&2
fi

if (( added > 0 )); then
  cat <<'TXT'
Langkah lanjutan untuk migration baru:
  - terapkan juga ke mudah_test_template (uji integrasi): db/migrate.sh --no-backup <nama>
  - kolom baru/berubah: perbarui query sqlx, crates/mz-audit/src/columns.rs, dan
    crates/mz-admin-jobs/src/company_data/dmmf.json + tests/golden/scope_plan.json
TXT
fi
(( changed == 0 ))
