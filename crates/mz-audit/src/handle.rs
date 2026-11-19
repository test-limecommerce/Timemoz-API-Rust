//! `AuditHandle` — padanan `RequestContextService` + middleware Prisma untuk pemanggil.
//!
//! Karena sqlx tidak punya middleware, pemanggil yang menulis model ber-audit memanggil
//! handle ini secara eksplisit:
//!
//! 1. SEBELUM tulisan: [`AuditHandle::pre_read`] / [`AuditHandle::pre_read_many`]
//!    (pra-baca lewat pool, sama seperti Node yang membaca lewat root client), atau
//!    [`AuditHandle::membership_pre`] untuk tabel keanggotaan.
//! 2. Tulisan pemanggil (boleh di dalam transaksi).
//! 3. SESUDAH tulisan berhasil: `record_*` dengan baris hasil tulisan (bentuk = hasil
//!    Prisma: tanpa `select` → [`AuditHandle::result_row`]; dengan `select` → baris
//!    berisi kolom yang sama).
//!
//! Tidak satu pun method di sini menggagalkan pemanggilnya (§0.21, §1.2): kegagalan
//! di-`tracing::warn` dan barisnya dicatat seadanya, persis seperti Node.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, Uri};
use mz_core::{AppState, RequestCtx};
use serde_json::Value;
use sqlx::{PgConnection, PgExecutor, PgPool};

use crate::build::{
    self, AuditEntry, Built, MemberEffect, PreRead, hydrate, hydrate_ids, key_of_row,
    member_has_soft_delete, member_select, members_of, membership_entry, next_keys, same_members,
    split_update_data,
};
use crate::read::{Columns, Lookup, read_rows};
use crate::row::Row;
use crate::scope::{
    AUDIT_BULK_THRESHOLD, ActorType, CompanionMode, CompanionSpec, EntityType, Model, Spec,
    parent_company_id_of, parent_label_of,
};
use crate::sink::{
    self, MAX_ACTOR_NAME, MAX_IP, MAX_ROUTE, MAX_USER_AGENT, WriteContext, truncate,
};

/// Batas tunggu pembacaan audit (`AUDIT_READ_IN_TX_TIMEOUT_MS`).
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Sakelar mati §3.6: hanya `AUDIT_LOG_ENABLED=false` (persis) yang mematikan audit.
/// Dibaca sekali per proses, seperti Node yang membacanya saat memasang middleware.
pub fn is_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("AUDIT_LOG_ENABLED").map_or(true, |v| v != "false"))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `RequestContextStore`.
pub(crate) struct Scope {
    pub request_id: Option<String>,
    pub route: Option<String>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    /// Diisi hanya oleh `run_as`.
    pub actor_type: Option<ActorType>,
    pub actor_name: Option<String>,
    /// `req.user?.id` — diisi extractor dari `RequestCtx`.
    pub actor_id: Mutex<Option<i32>>,
    /// `store.operator` (OperatorAuditInterceptor).
    pub operator: Mutex<Option<(i32, String)>>,
    /// `None` = tanpa buffer: entri ditulis saat itu juga (§0.28).
    pub buffer: Mutex<Option<Vec<AuditEntry>>>,
    /// Sesudah flush: entri yang datang terlambat tidak pernah ditulis (Node: buffer
    /// pengganti yang tidak pernah di-flush).
    pub closed: AtomicBool,
    pub pool: OnceLock<PgPool>,
}

impl Scope {
    fn new(buffered: bool) -> Self {
        Self {
            request_id: None,
            route: None,
            ip_address: None,
            user_agent: None,
            actor_type: None,
            actor_name: None,
            actor_id: Mutex::new(None),
            operator: Mutex::new(None),
            buffer: Mutex::new(buffered.then(Vec::new)),
            closed: AtomicBool::new(false),
            pool: OnceLock::new(),
        }
    }

    /// `createRequestStore(req)`.
    pub(crate) fn for_request(
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        peer: Option<std::net::SocketAddr>,
    ) -> Self {
        let mut s = Self::new(true);
        s.request_id = Some(uuid::Uuid::new_v4().to_string());
        s.route = truncate(Some(format!("{method} {}", uri.path()).trim()), MAX_ROUTE);
        s.ip_address = truncate(
            mz_core::rate_limit::client_ip(headers, peer).as_deref(),
            MAX_IP,
        );
        // Header Node didekode latin1.
        let ua = headers.get(http::header::USER_AGENT).map(|v| {
            v.as_bytes()
                .iter()
                .map(|b| char::from(*b))
                .collect::<String>()
        });
        s.user_agent = truncate(ua.as_deref(), MAX_USER_AGENT);
        s
    }

    /// `buildWriteContext(store)`.
    pub(crate) fn write_context(&self) -> WriteContext {
        if let Some((_, name)) = lock(&self.operator).clone() {
            return WriteContext {
                request_id: self.request_id.clone(),
                actor_type: ActorType::System,
                actor_id: None,
                actor_name: truncate(Some(&format!("Timemoz Support ({name})")), MAX_ACTOR_NAME),
                route: self.route.clone(),
                ip_address: self.ip_address.clone(),
                user_agent: self.user_agent.clone(),
            };
        }
        let actor_id = *lock(&self.actor_id);
        let actor_type = self.actor_type.unwrap_or(if actor_id.is_some() {
            ActorType::User
        } else {
            ActorType::System
        });
        WriteContext {
            request_id: self.request_id.clone(),
            actor_type,
            actor_id: if actor_type == ActorType::User {
                actor_id
            } else {
                None
            },
            actor_name: self.actor_name.clone(),
            route: self.route.clone(),
            ip_address: self.ip_address.clone(),
            user_agent: self.user_agent.clone(),
        }
    }

    /// Mengosongkan buffer untuk ditulis (`flushStore`), lalu menutup scope.
    pub(crate) fn take_for_flush(&self) -> Vec<AuditEntry> {
        self.closed.store(true, Ordering::SeqCst);
        lock(&self.buffer)
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }
}

/// Ekstensi request yang dipasang [`crate::layer()`].
#[derive(Clone)]
pub(crate) struct ScopeExt(pub Arc<Scope>);

/// Handle audit untuk satu request (atau satu jalur tanpa request). Murah di-clone.
#[derive(Clone)]
pub struct AuditHandle {
    pool: PgPool,
    scope: Arc<Scope>,
    enabled: bool,
}

/// Tabel keanggotaan: hasil [`AuditHandle::membership_pre`].
#[derive(Debug, Clone)]
pub struct MembershipPre {
    model: Model,
    effect: MemberEffect,
    state: MembershipState,
}

#[derive(Debug, Clone)]
enum MembershipState {
    Disabled,
    Failed,
    Ok {
        rows: Vec<Row>,
        before: HashMap<i32, Vec<Value>>,
    },
}

/// Baris keanggotaan yang tersentuh.
#[derive(Debug, Clone)]
pub enum Touched {
    /// `create`/`createMany`: argumen `data` (wajib memuat kolom induk + kolom kunci anggota).
    Rows(Vec<Row>),
    /// `update*`/`delete*`: `where` tulisan itu, dibaca lewat pool sebelum menulis.
    Where(Lookup),
}

impl AuditHandle {
    fn with_scope(pool: PgPool, scope: Scope) -> Self {
        Self {
            pool,
            scope: Arc::new(scope),
            enabled: is_enabled(),
        }
    }

    /// `runAs({ actorType, actorName }, fn)` — jalur tanpa request (cron, worker,
    /// backfill): `requestId` baru, tanpa buffer, setiap entri ditulis saat itu juga.
    pub fn run_as(pool: PgPool, actor_type: ActorType, actor_name: Option<&str>) -> Self {
        let mut s = Scope::new(false);
        s.request_id = Some(uuid::Uuid::new_v4().to_string());
        s.actor_type = Some(actor_type);
        s.actor_name = truncate(actor_name, MAX_ACTOR_NAME);
        Self::with_scope(pool, s)
    }

    /// `pushAudit` tanpa konteks sama sekali: ditulis langsung sebagai `system`,
    /// `requestId` NULL.
    pub fn detached(pool: PgPool) -> Self {
        Self::with_scope(pool, Scope::new(false))
    }

    /// Handle untuk request yang sedang berjalan. Dipakai extractor; dengan
    /// [`crate::layer()`] terpasang entri ditahan sampai respons dibuat.
    pub fn from_parts(parts: &Parts, pool: PgPool) -> Self {
        let scope = match parts.extensions.get::<ScopeExt>() {
            Some(ext) => ext.0.clone(),
            None => {
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "mz_audit::layer() tidak terpasang — entri audit ditulis langsung tanpa buffer request"
                    );
                }
                let peer = parts
                    .extensions
                    .get::<ConnectInfo<std::net::SocketAddr>>()
                    .map(|c| c.0);
                let mut s = Scope::for_request(&parts.method, &parts.uri, &parts.headers, peer);
                *s.buffer
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                Arc::new(s)
            }
        };
        let _ = scope.pool.set(pool.clone());
        if let Some(uid) = parts
            .extensions
            .get::<RequestCtx>()
            .and_then(RequestCtx::user_id)
        {
            *lock(&scope.actor_id) = Some(uid);
        }
        Self {
            pool,
            scope,
            enabled: is_enabled(),
        }
    }

    /// Handle yang tidak pernah menyentuh DB (test / audit dimatikan).
    pub fn disabled(pool: PgPool) -> Self {
        Self {
            pool,
            scope: Arc::new(Scope::new(false)),
            enabled: false,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Pelaku eksplisit (`req.user.id`) — untuk pemanggil yang tidak memakai extractor.
    pub fn set_actor(&self, user_id: Option<i32>) {
        *lock(&self.scope.actor_id) = user_id;
    }

    /// `store.operator` — request operator Timemoz ditulis `system` "Timemoz Support (<nama>)".
    pub fn set_operator(&self, id: i32, name: &str) {
        *lock(&self.scope.operator) = Some((id, name.to_owned()));
    }

    /// `discardAudit()` — dipanggil dari jalur rollback pemilik transaksi (§9.1).
    pub fn discard(&self) {
        if let Some(buf) = lock(&self.scope.buffer).as_mut() {
            buf.clear();
        }
    }

    /// Penjaga `.catch(() => discardAudit())`: kalau di-drop tanpa [`DiscardGuard::disarm`]
    /// (mis. `?` keluar dari transaksi), seluruh buffer request dibuang.
    pub fn discard_guard(&self) -> DiscardGuard<'_> {
        DiscardGuard {
            audit: self,
            armed: true,
        }
    }

    /// Konteks tulis saat ini (`buildWriteContext`).
    pub fn write_context(&self) -> WriteContext {
        self.scope.write_context()
    }

    /// Isi buffer saat ini (untuk test/diagnostik).
    pub fn buffered(&self) -> Vec<AuditEntry> {
        lock(&self.scope.buffer).clone().unwrap_or_default()
    }

    // ── pembacaan ────────────────────────────────────────────────────────────

    async fn read_pool(
        &self,
        model: Model,
        cols: Columns<'_>,
        lookup: &Lookup,
        extra: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<Row>, String> {
        match tokio::time::timeout(
            READ_TIMEOUT,
            read_rows(&self.pool, model, cols, lookup, extra, limit),
        )
        .await
        {
            Ok(Ok(rows)) => Ok(rows),
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err(format!(
                "pembacaan audit melewati {}ms",
                READ_TIMEOUT.as_millis()
            )),
        }
    }

    /// Pra-baca `update`/`upsert`/`delete` (`findFirst({ where, select })`) lewat pool.
    pub async fn pre_read(&self, model: Model, lookup: Lookup) -> PreRead {
        self.pre_read_inner(model, lookup, 1, "update").await
    }

    /// Pra-baca `updateMany`/`deleteMany` (`findMany({ where, take: 501 })`) lewat pool.
    pub async fn pre_read_many(&self, model: Model, lookup: Lookup) -> PreRead {
        self.pre_read_inner(model, lookup, AUDIT_BULK_THRESHOLD as i64 + 1, "updateMany")
            .await
    }

    async fn pre_read_inner(&self, model: Model, lookup: Lookup, limit: i64, op: &str) -> PreRead {
        if !self.enabled {
            return PreRead::Disabled;
        }
        match self
            .read_pool(model, Columns::Snapshot, &lookup, None, Some(limit))
            .await
        {
            Ok(rows) => PreRead::rows(rows),
            Err(err) => {
                tracing::warn!(
                    "Pra-baca audit gagal untuk {model}.{op}, barisnya dicatat tanpa before: {err}"
                );
                PreRead::Failed
            }
        }
    }

    /// Baris hasil tulisan seperti dikembalikan Prisma TANPA `select` (semua kolom
    /// skalar), dibaca lewat executor pemanggil (mis. transaksinya). `None` bila audit
    /// mati, baris tidak ada, atau pembacaan gagal (di-`warn`).
    pub async fn result_row<'e>(
        &self,
        exec: impl PgExecutor<'e>,
        model: Model,
        lookup: Lookup,
    ) -> Option<Row> {
        if !self.enabled {
            return None;
        }
        match read_rows(exec, model, Columns::All, &lookup, None, Some(1)).await {
            Ok(rows) => rows.into_iter().next(),
            Err(err) => {
                tracing::warn!("Pembacaan hasil tulisan {model} untuk audit gagal: {err}");
                None
            }
        }
    }

    // ── perekaman baris (§4.1, pendamping `mode: 'row'`) ─────────────────────

    /// `create` — `result` = baris yang dikembalikan `create` (sesuai `select`-nya).
    pub async fn record_create(&self, model: Model, result: &Row) {
        if self.enabled {
            self.finish(model, build::build_create(model, result)).await;
        }
    }

    /// `createMany` — `data` = argumen `data`, `affected` = `count`.
    pub async fn record_create_many(&self, model: Model, data: &[Row], affected: u64) {
        if self.enabled {
            self.finish(model, build::build_create_many(model, data, affected))
                .await;
        }
    }

    /// `update` — `pre` dari [`Self::pre_read`], `result` = baris hasil `update`.
    pub async fn record_update(&self, model: Model, pre: PreRead, result: &Row) {
        if self.enabled && pre != PreRead::Disabled {
            self.finish(model, build::build_update(model, &pre, result, false))
                .await;
        }
    }

    /// `upsert` — pra-baca yang berhasil tanpa baris = penyisipan (`create`).
    pub async fn record_upsert(&self, model: Model, pre: PreRead, result: &Row) {
        if self.enabled && pre != PreRead::Disabled {
            self.finish(model, build::build_update(model, &pre, result, true))
                .await;
        }
    }

    /// `updateMany` — `data` = argumen `data` Prisma (operasi seperti `{ "increment": 1 }`
    /// ditulis apa adanya; nilainya dibaca ulang per id), `where_` = argumen `where`
    /// (hanya dipakai baris ringkasan massal).
    pub async fn record_update_many(
        &self,
        model: Model,
        pre: PreRead,
        affected: u64,
        data: &Row,
        where_: Value,
    ) {
        if !self.enabled || pre == PreRead::Disabled {
            return;
        }
        let (plain, pending) = split_update_data(model, data);
        let mut post = HashMap::new();
        let bulk = !pre.is_ok()
            || affected as usize > AUDIT_BULK_THRESHOLD
            || matches!(
                pre,
                PreRead::Rows {
                    truncated: true,
                    ..
                }
            );
        if affected > 0 && !bulk && !pending.is_empty() {
            let ids: Vec<i32> = pre
                .as_rows()
                .iter()
                .filter_map(|r| r.int("id"))
                .filter_map(|i| i32::try_from(i).ok())
                .collect();
            if !ids.is_empty() {
                match self
                    .read_pool(model, Columns::Snapshot, &Lookup::Ids(ids), None, None)
                    .await
                {
                    Ok(rows) => {
                        for r in rows {
                            if let Some(id) = r.int("id") {
                                post.insert(id, r);
                            }
                        }
                    }
                    Err(err) => tracing::warn!("Pembacaan ulang audit gagal untuk {model}: {err}"),
                }
            }
        }
        self.finish(
            model,
            build::build_update_many(model, &pre, affected, &plain, &pending, &post, &where_),
        )
        .await;
    }

    /// `delete` — `result` = baris yang dikembalikan `delete` (pengganti pra-baca gagal).
    pub async fn record_delete(&self, model: Model, pre: PreRead, result: Option<&Row>) {
        if self.enabled && pre != PreRead::Disabled {
            self.finish(model, build::build_delete(model, &pre, result))
                .await;
        }
    }

    /// `deleteMany`.
    pub async fn record_delete_many(
        &self,
        model: Model,
        pre: PreRead,
        affected: u64,
        where_: Value,
    ) {
        if self.enabled && pre != PreRead::Disabled {
            self.finish(
                model,
                build::build_delete_many(model, &pre, affected, &where_),
            )
            .await;
        }
    }

    /// Ekor `buildRowEntries`: identitas induk / kode toko, lalu push.
    async fn finish(&self, model: Model, mut built: Vec<Built>) {
        if built.is_empty() {
            return;
        }
        match model.spec() {
            Spec::Companion(c) => {
                let mut entries: Vec<&mut AuditEntry> =
                    built.iter_mut().map(|b| &mut b.entry).collect();
                self.fill_parent_identity(&c, &mut entries).await;
            }
            Spec::Entity(e) if e.label_needs_store_code => self.append_store_code(&mut built).await,
            Spec::Entity(_) => {}
        }
        for b in built {
            self.push(b.entry).await;
        }
    }

    /// `fillParentIdentity` — `entityUuid`/`entityLabel`/`companyId` dari induk.
    async fn fill_parent_identity(
        &self,
        companion: &CompanionSpec,
        entries: &mut [&mut AuditEntry],
    ) {
        let mut ids: Vec<i32> = Vec::new();
        for e in entries.iter() {
            if !ids.contains(&e.entity_id) {
                ids.push(e.entity_id);
            }
        }
        if ids.is_empty() {
            return;
        }
        let parent = companion.parent_model;
        match self
            .read_pool(
                parent,
                Columns::Only(parent.parent_select()),
                &Lookup::Ids(ids),
                None,
                None,
            )
            .await
        {
            Ok(parents) => {
                for e in entries.iter_mut() {
                    let Some(p) = parents
                        .iter()
                        .find(|p| p.int("id") == Some(i64::from(e.entity_id)))
                    else {
                        continue;
                    };
                    e.entity_uuid = p.get("uuid").and_then(Value::as_str).map(str::to_owned);
                    e.entity_label = parent_label_of(parent, p);
                    if e.company_id.is_none() {
                        e.company_id =
                            parent_company_id_of(parent, p).and_then(|c| i32::try_from(c).ok());
                    }
                }
            }
            Err(err) => tracing::warn!(
                "Pembacaan induk {parent} gagal, baris audit ditulis tanpa entityUuid: {err}"
            ),
        }
    }

    /// `appendStoreCode` — label jadwal = `visitDate — store.code`.
    async fn append_store_code(&self, built: &mut [Built]) {
        let mut ids: Vec<i32> = Vec::new();
        for b in built.iter() {
            if let Some(id) = b.raw.int("storeId").and_then(|i| i32::try_from(i).ok())
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return;
        }
        match self
            .read_pool(
                Model::Store,
                Columns::Only(&["id", "code"]),
                &Lookup::Ids(ids),
                None,
                None,
            )
            .await
        {
            Ok(stores) => {
                for b in built.iter_mut() {
                    let Some(store_id) = b.raw.int("storeId") else {
                        continue;
                    };
                    let code = stores
                        .iter()
                        .find(|s| s.int("id") == Some(store_id))
                        .and_then(|s| s.text("code"));
                    if let (Some(code), Some(label)) = (code, b.entry.entity_label.as_ref()) {
                        b.entry.entity_label = Some(format!("{label} — {code}"));
                    }
                }
            }
            Err(err) => {
                tracing::warn!("Pembacaan kode toko gagal, label jadwal ditulis tanpa kode: {err}")
            }
        }
    }

    // ── keanggotaan (§4.2) ───────────────────────────────────────────────────

    /// Pra-baca keanggotaan (`preReadMembership`): baris yang tersentuh DAN daftar
    /// anggota aktif tiap induk SEBELUM penyuntingan. Panggil sebelum menulis.
    /// `update*` yang tidak menyentuh `deletedAt` tidak mengubah keanggotaan: jangan
    /// panggil sama sekali.
    pub async fn membership_pre(
        &self,
        model: Model,
        effect: MemberEffect,
        touched: Touched,
    ) -> MembershipPre {
        let disabled = MembershipPre {
            model,
            effect,
            state: MembershipState::Disabled,
        };
        let Some(companion) = model
            .companion_spec()
            .filter(|c| c.mode == CompanionMode::Membership)
        else {
            tracing::warn!("membership_pre dipanggil untuk {model} yang bukan tabel keanggotaan");
            return disabled;
        };
        if !self.enabled {
            return disabled;
        }
        let kind = companion.member_kind;
        let rows = match touched {
            Touched::Rows(rows) => rows,
            Touched::Where(lookup) => {
                let mut select: Vec<&str> = member_select(kind).to_vec();
                select.push(companion.parent_id_field);
                match self
                    .read_pool(model, Columns::Only(&select), &lookup, None, None)
                    .await
                {
                    Ok(rows) => rows,
                    Err(err) => {
                        tracing::warn!(
                            "Pra-baca keanggotaan {model} gagal, perubahannya tidak tercatat: {err}"
                        );
                        return MembershipPre {
                            model,
                            effect,
                            state: MembershipState::Failed,
                        };
                    }
                }
            }
        };
        let mut before = HashMap::new();
        for parent_id in parent_ids(&rows, companion.parent_id_field) {
            if self
                .find_buffered(companion.entity_type, parent_id, companion.sub_entity)
                .is_some()
            {
                continue;
            }
            before.insert(
                parent_id,
                self.read_members(model, &companion, parent_id).await,
            );
        }
        MembershipPre {
            model,
            effect,
            state: MembershipState::Ok { rows, before },
        }
    }

    /// `readMembers` — anggota aktif satu induk, siap tulis.
    async fn read_members(
        &self,
        model: Model,
        companion: &CompanionSpec,
        parent_id: i32,
    ) -> Vec<Value> {
        let kind = companion.member_kind;
        let field = companion.parent_id_field;
        let lookup = Lookup::filter(move |qb| {
            qb.push(format!(r#""{field}" = "#)).push_bind(parent_id);
        });
        let extra = member_has_soft_delete(kind).then_some(r#""deletedAt" IS NULL"#);
        match self
            .read_pool(
                model,
                Columns::Only(member_select(kind)),
                &lookup,
                extra,
                None,
            )
            .await
        {
            Ok(rows) => {
                let keys: Vec<String> = rows.iter().filter_map(|r| key_of_row(kind, r)).collect();
                self.hydrate(kind, &keys).await
            }
            Err(err) => {
                tracing::warn!(
                    "Pra-baca anggota {model} gagal, baris audit ditulis tanpa before: {err}"
                );
                Vec::new()
            }
        }
    }

    /// `hydrate` + `withNames` (nama dibaca lewat pool).
    async fn hydrate(&self, kind: crate::scope::MemberKind, keys: &[String]) -> Vec<Value> {
        let ids = hydrate_ids(kind, keys);
        if ids.is_empty() {
            return hydrate(kind, keys, Some(&HashMap::new()));
        }
        let ids32: Vec<i32> = ids.iter().filter_map(|i| i32::try_from(*i).ok()).collect();
        match self
            .read_pool(
                Model::Users,
                Columns::Only(&["id", "name"]),
                &Lookup::Ids(ids32),
                None,
                None,
            )
            .await
        {
            Ok(rows) => {
                let names: HashMap<i64, Option<String>> = rows
                    .iter()
                    .filter_map(|r| {
                        Some((
                            r.int("id")?,
                            r.get("name").and_then(Value::as_str).map(str::to_owned),
                        ))
                    })
                    .collect();
                hydrate(kind, keys, Some(&names))
            }
            Err(err) => {
                tracing::warn!("Pembacaan nama anggota gagal, daftar ditulis tanpa nama: {err}");
                hydrate(kind, keys, None)
            }
        }
    }

    /// `buildMembershipEntries` — panggil SESUDAH tulisan berhasil. Satu baris `update`
    /// per induk per request (digabung dengan entri yang sudah menunggu di buffer).
    pub async fn record_membership(&self, pre: MembershipPre) {
        let MembershipState::Ok {
            rows,
            before: members_before,
        } = pre.state
        else {
            return;
        };
        let Some(companion) = pre.model.companion_spec() else {
            return;
        };
        let kind = companion.member_kind;
        let mut fresh: Vec<AuditEntry> = Vec::new();

        for parent_id in parent_ids(&rows, companion.parent_id_field) {
            let buffered =
                self.find_buffered(companion.entity_type, parent_id, companion.sub_entity);
            let (before, base) = match &buffered {
                Some(b) => (members_of(b.before.as_ref()), members_of(b.after.as_ref())),
                None => {
                    let before = members_before.get(&parent_id).cloned().unwrap_or_default();
                    (before.clone(), before)
                }
            };
            let touched: Vec<String> = rows
                .iter()
                .filter(|r| r.int(companion.parent_id_field) == Some(i64::from(parent_id)))
                .filter_map(|r| key_of_row(kind, r))
                .collect();
            let keys = next_keys(kind, &base, &touched, pre.effect);
            let after = self.hydrate(kind, &keys).await;

            if same_members(kind, &before, &after) {
                if buffered.is_some() {
                    self.remove_buffered(companion.entity_type, parent_id, companion.sub_entity);
                }
                continue;
            }
            if buffered.is_some() {
                self.update_buffered(
                    companion.entity_type,
                    parent_id,
                    companion.sub_entity,
                    after,
                );
                continue;
            }
            fresh.push(membership_entry(
                companion.entity_type,
                parent_id,
                companion.sub_entity,
                before,
                after,
            ));
        }

        if !fresh.is_empty() {
            let mut refs: Vec<&mut AuditEntry> = fresh.iter_mut().collect();
            self.fill_parent_identity(&companion, &mut refs).await;
        }
        for e in fresh {
            self.push(e).await;
        }
    }

    fn matches(e: &AuditEntry, et: EntityType, id: i32, sub: &str) -> bool {
        e.entity_type == et && e.entity_id == id && e.sub_entity.as_deref() == Some(sub)
    }

    /// `findBufferedAudit`.
    fn find_buffered(&self, et: EntityType, id: i32, sub: &str) -> Option<AuditEntry> {
        lock(&self.scope.buffer)
            .as_ref()?
            .iter()
            .find(|e| Self::matches(e, et, id, sub))
            .cloned()
    }

    /// `removeBufferedAudit`.
    fn remove_buffered(&self, et: EntityType, id: i32, sub: &str) {
        if let Some(buf) = lock(&self.scope.buffer).as_mut()
            && let Some(idx) = buf.iter().position(|e| Self::matches(e, et, id, sub))
        {
            buf.remove(idx);
        }
    }

    fn update_buffered(&self, et: EntityType, id: i32, sub: &str, after: Vec<Value>) {
        if let Some(buf) = lock(&self.scope.buffer).as_mut()
            && let Some(e) = buf.iter_mut().find(|e| Self::matches(e, et, id, sub))
        {
            e.after = Some(serde_json::json!({ "members": after }));
            e.changed_fields = vec!["members".to_owned()];
        }
    }

    // ── penulisan ────────────────────────────────────────────────────────────

    /// `pushAudit(entry)`: ada buffer → ditahan sampai respons; tanpa buffer → ditulis
    /// saat itu juga (kegagalan di-`warn`).
    pub async fn push(&self, entry: AuditEntry) {
        if !self.enabled {
            return;
        }
        {
            let mut guard = lock(&self.scope.buffer);
            if let Some(buf) = guard.as_mut() {
                if self.scope.closed.load(Ordering::SeqCst) {
                    tracing::debug!(
                        "entri audit datang sesudah flush, dibuang: {}#{}",
                        entry.entity_type,
                        entry.entity_id
                    );
                } else {
                    buf.push(entry);
                }
                return;
            }
        }
        let ctx = self.scope.write_context();
        if let Err(err) = sink::write(&self.pool, std::slice::from_ref(&entry), &ctx).await {
            tracing::warn!("Gagal menulis audit tanpa konteks request: {err}");
        }
    }

    /// `recordWithin(client, entry)` — satu entri ditulis DI DALAM transaksi pemanggil
    /// (batal bersama transaksinya); mengembalikan `uuid` baris yang lahir.
    pub async fn record_within(
        &self,
        conn: &mut PgConnection,
        entry: AuditEntry,
    ) -> sqlx::Result<Option<String>> {
        if !self.enabled {
            return Ok(None);
        }
        let ctx = self.scope.write_context();
        let rows = sink::build_rows(
            conn,
            std::slice::from_ref(&entry),
            &ctx,
            sink::Lookup::InTransaction,
        )
        .await?;
        Ok(sink::insert_rows(conn, &rows).await?.into_iter().next())
    }
}

/// Lihat [`AuditHandle::discard_guard`].
#[must_use = "penjaga yang langsung di-drop langsung membuang buffer"]
pub struct DiscardGuard<'a> {
    audit: &'a AuditHandle,
    armed: bool,
}

impl DiscardGuard<'_> {
    /// Jalur sukses (transaksi commit / callback kembali tanpa melempar).
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for DiscardGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.audit.discard();
        }
    }
}

/// `distinct(rows.map(row => asInt(row[parentIdField])))`.
fn parent_ids(rows: &[Row], field: &str) -> Vec<i32> {
    let mut out = Vec::new();
    for id in rows
        .iter()
        .filter_map(|r| r.int(field))
        .filter_map(|i| i32::try_from(i).ok())
    {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

impl FromRequestParts<AppState> for AuditHandle {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(AuditHandle::from_parts(parts, state.db.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::Action;

    fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(50))
            .connect_lazy("postgres://u:p@127.0.0.1:1/x")
            .unwrap_or_else(|_| unreachable_pool())
    }

    fn unreachable_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy_with(sqlx::postgres::PgConnectOptions::new())
    }

    fn scoped() -> AuditHandle {
        let scope = Scope::for_request(
            &Method::PATCH,
            &"/users/abc?x=1".parse().unwrap_or_default(),
            &HeaderMap::new(),
            Some(
                "10.0.0.1:5000"
                    .parse()
                    .unwrap_or_else(|_| std::net::SocketAddr::from(([0, 0, 0, 0], 0))),
            ),
        );
        AuditHandle {
            pool: lazy_pool(),
            scope: Arc::new(scope),
            enabled: true,
        }
    }

    fn entry(id: i32) -> AuditEntry {
        membership_entry(
            EntityType::Division,
            id,
            "divisionMember",
            vec![],
            vec![serde_json::json!({"id": 1})],
        )
    }

    #[tokio::test]
    async fn request_scope_traces_route_without_query() {
        let h = scoped();
        let ctx = h.write_context();
        assert_eq!(ctx.route.as_deref(), Some("PATCH /users/abc"));
        assert_eq!(ctx.ip_address.as_deref(), Some("::ffff:10.0.0.1"));
        assert!(
            ctx.request_id
                .as_deref()
                .is_some_and(|r| uuid::Uuid::parse_str(r).is_ok())
        );
        assert_eq!(ctx.actor_type, ActorType::System);
        h.set_actor(Some(5));
        let ctx = h.write_context();
        assert_eq!((ctx.actor_type, ctx.actor_id), (ActorType::User, Some(5)));
        h.set_operator(9, "Rina");
        let ctx = h.write_context();
        assert_eq!(ctx.actor_type, ActorType::System);
        assert_eq!(ctx.actor_id, None);
        assert_eq!(ctx.actor_name.as_deref(), Some("Timemoz Support (Rina)"));
    }

    #[tokio::test]
    async fn buffer_push_discard_and_flush_once() {
        let h = scoped();
        h.push(entry(1)).await;
        h.push(entry(2)).await;
        assert_eq!(h.buffered().len(), 2);
        h.discard();
        assert!(h.buffered().is_empty());
        h.push(entry(3)).await;
        let flushed = h.scope.take_for_flush();
        assert_eq!(flushed.len(), 1);
        // Flush kedua tidak menulis ulang; entri terlambat dibuang.
        assert!(h.scope.take_for_flush().is_empty());
        h.push(entry(4)).await;
        assert!(h.buffered().is_empty());
    }

    #[tokio::test]
    async fn discard_guard_clears_only_when_armed() {
        let h = scoped();
        h.push(entry(1)).await;
        h.discard_guard().disarm();
        assert_eq!(h.buffered().len(), 1);
        {
            let _guard = h.discard_guard();
        }
        assert!(h.buffered().is_empty());
    }

    #[tokio::test]
    async fn buffered_entries_can_be_found_updated_and_removed() {
        let h = scoped();
        h.push(entry(7)).await;
        assert!(
            h.find_buffered(EntityType::Division, 7, "divisionMember")
                .is_some()
        );
        assert!(h.find_buffered(EntityType::Division, 7, "member").is_none());
        h.update_buffered(EntityType::Division, 7, "divisionMember", vec![]);
        assert_eq!(
            h.find_buffered(EntityType::Division, 7, "divisionMember")
                .and_then(|e| e.after),
            Some(serde_json::json!({"members": []}))
        );
        h.remove_buffered(EntityType::Division, 7, "divisionMember");
        assert!(h.buffered().is_empty());
    }

    #[tokio::test]
    async fn run_as_labels_system_and_has_no_buffer() {
        let h = AuditHandle::run_as(lazy_pool(), ActorType::System, Some("cron"));
        let ctx = h.write_context();
        assert_eq!(ctx.actor_type, ActorType::System);
        assert_eq!(ctx.actor_name.as_deref(), Some("cron"));
        assert!(ctx.request_id.is_some());
        assert!(
            h.find_buffered(EntityType::Division, 1, "divisionMember")
                .is_none()
        );
        let d = AuditHandle::detached(lazy_pool());
        assert_eq!(d.write_context().request_id, None);
    }

    #[tokio::test]
    async fn disabled_handle_skips_everything() {
        let h = AuditHandle::disabled(lazy_pool());
        assert_eq!(
            h.pre_read(Model::Users, Lookup::Id(1)).await,
            PreRead::Disabled
        );
        h.push(entry(1)).await;
        assert!(h.buffered().is_empty());
        let _ = Action::Create;
    }

    #[tokio::test]
    async fn failed_pre_read_never_fails_the_caller() {
        let h = scoped();
        assert_eq!(
            h.pre_read(Model::Users, Lookup::Id(1)).await,
            PreRead::Failed
        );
        // Dicatat tanpa before (§9.6).
        let after = Row::new()
            .with("id", 1)
            .with("uuid", "u1")
            .with("username", "a")
            .with("name", "A")
            .with("status", 1);
        h.record_update(Model::Users, PreRead::Failed, &after).await;
        let buffered = h.buffered();
        assert_eq!(buffered.len(), 1);
        assert_eq!(buffered.first().map(|e| e.before.is_none()), Some(true));
    }
}
