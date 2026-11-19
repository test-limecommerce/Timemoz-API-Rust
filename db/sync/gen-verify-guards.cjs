// Menghasilkan db/verify-guards.sql dari prisma/apply-partial-indexes.js Node.
// Pakai (lewat db/sync-from-node.sh): node gen-verify-guards.cjs <repo Mudah-API> <keluaran .sql> <label commit>
const path = require('path');
const fs = require('fs');
const [, , nodeRepo, outFile, label] = process.argv;
const m = require(path.resolve(nodeRepo, 'prisma/apply-partial-indexes.js'));

const q = (s) => (s == null ? 'NULL' : "'" + String(s).replace(/'/g, "''") + "'");
const arr = (a) => (a && a.length ? 'ARRAY[' + a.map(q).join(', ') + ']' : 'ARRAY[]::text[]');
const rows = [];
const add = (kind, e) =>
  rows.push(
    `  (${rows.length + 1}, ${q(kind)}, ${q(e.table)}, ${q(e.name)}, ${q(e.expect ?? null)}, ` +
      `${e.unique === false ? 'false' : 'true'}, ${q(e.requires?.table ?? e.table)}, ${arr(e.requires?.columns)})`,
  );
for (const e of m.INDEXES) add('index', e);
for (const e of m.CONSTRAINTS) add('constraint', e);
for (const e of m.TRIGGERS) add('trigger', e);
for (const e of m.OBSOLETE_INDEXES) add('obsolete', e);

const out = `-- Verifikasi "DB guard": partial/expression/GIN/INCLUDE index, CHECK constraint, dan trigger
-- yang tidak bisa dinyatakan schema.prisma. Port verifyDbGuards() dari Mudah-API
-- prisma/apply-partial-indexes.js (daftar INDEXES/CONSTRAINTS/TRIGGERS/OBSOLETE_INDEXES
-- di ${label}). Hanya MEMBACA katalog; tidak mengubah apa pun.
--
-- Keluaran: satu baris per masalah, kolom status|jenis|nama|alasan.
--   MISSING  guard tidak terpasang atau definisinya tidak sesuai -> perbaiki
--   OBSOLETE index usang masih ada -> DROP INDEX CONCURRENTLY
--   PENDING  tabel/kolom prasyarat belum ada (migrasi belum diterapkan) -> info
-- Kosong = semua guard terpasang.
--
-- Definisi guard ada di migrasi yang membuatnya (db/migrations). Migrasi baru yang
-- menambah/mengubah guard WAJIB menambah/mengubah barisnya di sini juga
-- (kolom: urutan, jenis, tabel, nama, potongan teks yang wajib ada di definisinya,
-- index unique?, tabel prasyarat, kolom prasyarat).
--
-- Jalankan: db/migrate.sh --verify-guards

WITH guard(ord, kind, tbl, name, expect, is_unique, req_table, req_columns) AS (VALUES
${rows.join(',\n')}
),
state AS (
  SELECT g.*,
         to_regclass(quote_ident(g.tbl)) IS NOT NULL AS has_table,
         (SELECT r.c FROM unnest(g.req_columns) WITH ORDINALITY AS r(c, i)
           WHERE NOT EXISTS (
             SELECT 1 FROM information_schema.columns ic
              WHERE ic.table_schema = current_schema()
                AND ic.table_name = g.req_table AND ic.column_name = r.c)
           ORDER BY r.i LIMIT 1) AS missing_column,
         CASE g.kind
           WHEN 'constraint' THEN (
             SELECT pg_get_constraintdef(oid) FROM pg_constraint
              WHERE conname = g.name AND connamespace = current_schema()::regnamespace LIMIT 1)
           WHEN 'trigger' THEN (
             SELECT pg_get_triggerdef(oid) FROM pg_trigger
              WHERE tgname = g.name AND NOT tgisinternal LIMIT 1)
           ELSE (
             SELECT indexdef FROM pg_indexes
              WHERE schemaname = current_schema() AND indexname = g.name)
         END AS def
    FROM guard g
),
verdict AS (
  SELECT ord, kind, name,
         CASE
           WHEN kind = 'obsolete' THEN CASE WHEN def IS NOT NULL THEN 'OBSOLETE' END
           WHEN NOT has_table OR missing_column IS NOT NULL THEN 'PENDING'
           WHEN def IS NULL THEN 'MISSING'
           WHEN kind = 'index'
                AND ((is_unique AND strpos(def, 'UNIQUE INDEX') = 0) OR strpos(def, expect) = 0)
             THEN 'MISSING'
           WHEN kind = 'constraint' AND (strpos(def, 'CHECK') = 0 OR strpos(def, expect) = 0)
             THEN 'MISSING'
           WHEN kind = 'trigger' AND strpos(def, expect) = 0 THEN 'MISSING'
         END AS status,
         CASE
           WHEN kind = 'obsolete' THEN 'index usang masih ada di tabel "' || tbl || '"'
           WHEN NOT has_table THEN 'tabel "' || tbl || '" belum ada'
           WHEN missing_column IS NOT NULL THEN 'kolom "' || missing_column || '" belum ada'
           WHEN def IS NULL THEN 'tidak terpasang'
           ELSE 'definisinya tidak sesuai: ' || def
         END AS reason
    FROM state
)
SELECT status, kind, name, reason FROM verdict WHERE status IS NOT NULL ORDER BY ord;
`;
fs.writeFileSync(outFile, out);
console.log(`${rows.length} guard -> ${outFile}`);
