# mz-audit

Port sisi TULIS (+ BACA, lihat bawah) audit log Mudah-API: `src/audit-logs/{audit-log.middleware,audit-scope,audit-field-policy,audit-soft-delete-signature,audit-logs.service}.ts`
+ `src/request-context/*`. Baris `auditLog` yang dihasilkan identik dengan Node untuk perubahan yang sama
(kolom, `action`, `entityType`, `subEntity`, `before`/`after`/`changedFields`, `companyId`, pelaku, jejak request).

Sisi BACA (Fase 4a, `src/admin/`): 5 route `/v1/admin/audit-logs` (`mz_audit::router(&state)`), restore
(`POST /{uuid}/restore`, menulis baris `restore` di dalam transaksinya), label/diff (`mz_audit::labels`) dan cron
`audit-prune` (`mz_audit::cron_jobs()`, `mz_audit::prune_expired`). Daftar route + deviasi: `plan/deviations/mz-audit.md`.

## Pasang di `mz-server`

```rust
let api = Router::new()
    .merge(mz_auth::router(state))
    // ...
    .with_state(state.clone())
    .layer(mz_audit::layer())          // satu scope audit per request, flush SESUDAH respons
    .layer(DefaultBodyLimit::max(100 * 1024));
// serve dengan into_make_service_with_connect_info::<SocketAddr>() (IP fallback; sudah begitu).
```

`layer()` memasang buffer per request (requestId uuid v4, route `METHOD /path` ≤160, IP `client_ip` ≤45,
user-agent ≤255). Sesudah handler menghasilkan respons, isi buffer ditulis SATU `INSERT` multi-baris di
`tokio::spawn`; galat → `tracing::warn` (seperti `res.on('finish')` Node). `AUDIT_LOG_ENABLED=false` (persis)
mematikan semuanya (dibaca sekali per proses).

## Pakai di handler

```rust
use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Row, Touched};

pub async fn handler(State(state): State<AppState>, ctx: RequestCtx, audit: AuditHandle, /* body terakhir */) { ... }
```

`AuditHandle` adalah extractor (`FromRequestParts<AppState>`): ia mengikat pelaku = `RequestCtx::user_id()`
(= `req.user.id`), jadi ekstrak di handler mana pun yang (langsung/tak langsung) menulis model ber-audit, lalu
oper `&audit` ke service. Tanpa user (route publik) pelakunya `system`, sama seperti Node.

Pola per operasi Prisma (pemanggil meniru bentuk tulisan Node):

| Node | Rust |
|---|---|
| `x.create({ data })` tanpa `select` | tulis (`RETURNING id`) → `if let Some(r) = audit.result_row(&mut *tx, M, Lookup::Id(id)).await { audit.record_create(M, &r).await }` |
| `x.create({ data, select })` | `audit.record_create(M, &Row::new().with("id", id).with("uuid", u)...)` (kolom = `select`) |
| `x.createMany({ data })` | `audit.record_create_many(M, &rows_data, count)` (entitas → 1 baris `bulk`; pendamping dirinci) |
| `x.update({ where, data })` | `let pre = audit.pre_read(M, lookup).await;` tulis → `audit.record_update(M, pre, &after)` |
| `x.upsert` | `pre_read` → `record_upsert` |
| `x.updateMany({ where, data })` | `let pre = audit.pre_read_many(M, lookup).await;` tulis → `audit.record_update_many(M, pre, affected, &data, json!(where))` |
| `x.delete` / `deleteMany` | `pre_read`/`pre_read_many` → `record_delete(M, pre, Some(&deleted))` / `record_delete_many(..)` |
| tabel keanggotaan (`divisionMember`, `storeGroupUser`, `rolePermission`, `storeVisitFormReviser`, `manualTimeApprovalException`, `overtimeApprover`, `storeVisitFormTemplateUser`) | SEBELUM tulis: `let m = audit.membership_pre(M, MemberEffect::Add/Remove, Touched::Rows(data) / Touched::Where(lookup)).await;` SESUDAH: `audit.record_membership(m).await` |
| `requestContext.discardAudit()` | `audit.discard()` atau `let g = audit.discard_guard(); ... tx.commit()?; g.disarm();` |
| `recordWithin(tx, entry)` | `audit.record_within(&mut tx, entry).await? -> Option<uuid>` |
| `runAs({ actorType, actorName })` | `AuditHandle::run_as(pool, ActorType::System, Some("cron"))` (tanpa buffer, tulis langsung) |
| operator Timemoz | `audit.set_operator(id, name)` → `system` "Timemoz Support (name)" |

Aturan yang wajib diikuti agar hasilnya identik:

- **Pra-baca lewat pool, bukan transaksi** (`pre_read*`, `membership_pre`): Node membaca lewat root client. Akibatnya baris
  yang dibuat di transaksi yang sama belum terlihat (mis. induk `users` baru → `entityUuid`/`entityLabel` NULL) — sama dengan Node.
- **Bentuk `after` = hasil Prisma**: tanpa `select` → semua kolom (`result_row`, urutan = `schema.prisma`, penting untuk
  `changedFields` sebuah `create`); dengan `select` → hanya kolom itu (kolom lain yang tidak null di `before` ikut terhitung berubah).
- `data` `updateMany` ditulis seperti argumen Prisma: nilai polos, `{ "set": x }`, atau operasi (`{ "increment": 1 }` → dibaca ulang per id).
  Kolom rahasia (`password`, `*Token`) boleh tidak ikut — kebijakan kolom membuangnya.
- `record_*` dipanggil SESUDAH tulisan berhasil (sebelum commit pun boleh; rollback tanpa `discard()` tetap tercatat, persis Node).
- `nested write` Prisma (mis. `users.update({ data: { userCompany: { updateMany } } })`) TIDAK diaudit Node — jangan dicatat.
- Tidak ada method yang menggagalkan pemanggil: pra-baca gagal → `PreRead::Failed` (baris dicatat tanpa `before` / ringkasan `bulk`).

Cakupan model (`Model`): 33 entitas `AUDIT_ENTITY_MODELS` + 11 pendamping `AUDIT_COMPANION_MODELS`; kolom per model
digenerate dari `prisma/schema.prisma` (`python3 -I tools/gen_columns.py <schema> > src/columns.rs`).

Utilitas murni yang diekspor: `project_row`, `diff_snapshots`, `is_excluded_field`, `sanitize_where`, `detect_transition`,
`has_required_fields`, `read_rows` (pembaca snapshot ber-normalisasi Prisma).

Deviasi: `plan/deviations/mz-audit.md`.
