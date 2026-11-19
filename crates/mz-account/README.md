# mz-account

Helper domain akun yang dipakai BERSAMA `mz-auth` dan `mz-users`. Tiap modul = port satu berkas Node
(path relatif ke `Mudah-API/src`). Konvensi: fungsi yang butuh cache/mail/pool menerima `&AppState`;
fungsi yang HARUS di dalam transaksi pemanggil menerima `&mut PgConnection` (`&mut *tx`; tanpa
transaksi: `state.db.acquire().await?`). Galat = `mz_core::ApiError` identik dengan Node.
Pemanggil bertanggung jawab atas `commit` dan `state.invalidate_*` untuk fungsi `&mut PgConnection`.

## `password` — `auth/password-check.ts` + `bcrypt/bcrypt.factory.ts`
- `INVALID_CREDENTIALS_MESSAGE` — "Invalid username or password".
- `hash_password(state, plain) -> ApiResult<String>` — bcrypt (`spawn_blocking`), cost `config.bcrypt_cost`, hasil `$2b$`.
- `hash_with_cost(plain, cost)` — sama dengan cost eksplisit.
- `verify_password(plain, hash) -> bool` — hash Node `$2b$` terverifikasi; hash rusak = `false`.
- `compare_password_safely(state, password, Option<&str>) -> bool` — `comparePasswordSafely`; hash kosong/None tetap membayar satu bcrypt ke hash pengganti (timing-safe) lalu `false`.

## `invite_token` — `users/invite-token.ts` (format `v1.<exp>.<sig>`, HMAC identik dengan Node)
- `sign_invite_token_now(state, user_uuid) -> Result<String, InviteTokenSecretMissing>` (→ `ApiError` 500 via `?`); `sign_invite_token(secret, uuid, now_ms)`.
- `verify_invite_token_now(state, token, user_uuid) -> bool`; `verify_invite_token(secret, token, uuid, now_ms)`.
- `is_invite_token_format(token)`, `build_invite_url(state, username, token)`, `INVITE_TOKEN_TTL_SECONDS`.

## `invite_method` — `users/invite-method.ts`
- `EMAIL`, `LINK`, `to_invite_method(Option<&str>) -> Option<&'static str>`.

## `username_conflict` — `users/username-conflict.ts`
- `is_username_conflict(&sqlx::Error)`, `is_username_conflict_api(&ApiError)` — unique violation `users.username`.

## `manual_seat_limit` — `users/manual-seat-limit.ts`
- `assert_manual_seat_available(conn, company_id, requested: i64, exclude_user_id: Option<i32>)` — lock `seats:<id>` + 409 `SEAT_LIMIT_REACHED`; WAJIB di transaksi yang menulis `userCompany`.

## `random_password` — `users/random-password.ts`
- `generate_random_password(length)`, `generate_default_random_password()` (14 karakter).

## `assignable_role` — `permissions/assignable-role.ts` (+ `effective-role.ts` `redirectToCompanyOverride`)
- Tipe: `RoleActor`, `AssignableRoleActor::{SelfRegistration, Actor}`, `RoleRef { code, uuid }`, `AccessLevel` (baris `accessLevel` lengkap, JSON = Prisma), `AssignmentRow`.
- `role_actor_of(effective_role_code)`; `require_actor_role(conn, actor_id, company_id)` — jabatan tertinggi pelaku, 403 bila kosong.
- `resolve_assignable_role(conn, &actor, company_id, RoleRef)` — Aturan B (422 pesan identik); `redirect_to_company_override`; `can_assign(rank, code)`.
- `assert_target_within_rank(conn, &actor, target_id, company_id)`, `assert_targets_within_rank(conn, &actor, &[ids], company_id)`, `assert_rows_within_rank` — Aturan C (403 `TARGET_ABOVE_ACTOR`).
- `ACCESS_LEVEL_COLUMNS`, `TARGET_ABOVE_ACTOR`.

## `role_summary` — `permissions/role-summary.ts`
- `RoleSummary` (Serialize), `RoleSummaryRow` (FromRow), `ROLE_SUMMARY_COLUMNS` (SQL, alias `al`), `role_summary_of(Option<&Row>)`.

## `new_employee_default` — `manual-time-settings/new-employee-default.ts`
- `apply_new_employee_manual_time_default(conn, audit, company_id, user_id, actor_id: Option<i32>)` — di transaksi join.

## Seed company baru (semua `ON CONFLICT DO NOTHING`, panggil di transaksi registrasi)
- `customer_types` — `customers/default-customer-types.ts`: `DEFAULT_CUSTOMER_TYPES`, `DEFAULT_CUSTOMER_TYPE_CODE`, `seed_default_customer_types(conn, company_id, created_by) -> u64`.
- `uoms` — `products/default-uoms.ts`: `DEFAULT_UOMS` (63), `seed_default_uoms(conn, company_id, created_by) -> u64`.
- `visit_masters` — `visit-masters/default-visit-masters.ts`: `DEFAULT_VISIT_TYPES/FREQUENCIES`, `DEFAULT_VISIT_TYPE_CODE`, `seed_default_visit_types`, `seed_default_visit_frequencies`, `seed_default_visit_masters(conn, company_id, created_by) -> (u64, u64)`.

## `signup_tracking` — `analytics/signup-tracking.ts`
- `normalize_signup_attribution(&Value, now: DateTime<Utc>) -> SignupAttribution`, `compact_signup_attribution(&SignupAttribution) -> Option<Map>`.
- `classify_acquisition(signup_type, &attr) -> AcquisitionSummary`, `product_label(PricingProductType)`, `product_type_from_modules(time, sales)`.
- `build_sign_up_properties(&SignUpEventInput) -> Map<String, Value>` (untuk `mz_core::mixpanel::TrackInput.properties`); konstanta `SELF_SIGNUP | ADMIN_INVITED | JOIN_LINK`.

## `company_response` — `companies/company-response.ts`
- `to_company_response(Value) -> Value` (buang `profilePictureFullPath`), `HIDDEN_COMPANY_FIELDS`.

## `mail` — `mail/mail.service.ts` (di atas `mz_core::mail::send`; mengembalikan `MailResult`)
- `send_user_confirmation(state, audit, username, name, token)` (set `users.sentAt`), `send_email_forgot_password(state, username, name, token)` (cek+tandai token), `send_invite_email`, `send_invite_credentials_email`, `send_confirmation_success_email`, `send_account_confirmation`, `send_admin_register_info`, `send_personal_welcome(payload: Value)`.

## `user_ops` — `users/users.service.ts` (+ `find_one`, `target_user`, `division_membership`, `plan_boost`)
- `find_token(state, audit, token, username, password) -> TokenClaim { id, activated }` (:383), `confirm(state, user_id, activated) -> Value` (:280).
- `forgot_password(state, audit, forgot_password_uuid, password, username) -> Value` (:458), `change_password(state, audit, user_id, old, new, confirm) -> Value` (:531), `send_email_forgot_password(state, username) -> Value` (:610; mail di-spawn).
- `enable(state, audit, uuid, requester_id) -> Value` (:2171), `disable(state, audit, uuid, requester_id) -> Value` (:2210).
- `find_one(state, id) -> ApiResult<Option<Value>>` (:931, isi `GET /auth/me`; `None` = Node `null`).
- `target_user` (`users/resolve-target-user.ts`): `UserRow` (kolom `users` tanpa rahasia) + `USER_ROW_COLUMNS`, `find_company_member`, `require_company_member(&PgPool, actor, uuid)`, `assert_no_shared_account_change(&PgPool, actor, &UserRow, company_id, &SharedAccountDto)`, `TARGET_SHARED_ACCOUNT`.
- `division_membership` (`divisions/division-membership-hook.ts`): `remove_users_from_divisions_in_tx(conn, audit, &[ids], Option<company_id>)`, `lock_divisions_company`.
- `plan_boost` (`plan-limits/plan-boost.ts`): `apply_plan_boosts(&mut Map, &[String])`.

## Lain
- `error_codes` — `auth/auth-error-codes.ts` (konstanta `CONFIRM_LINK_INVALID`, `EMAIL_TAKEN`, ...).
- `util` — `encode_uri_component` (JS), `env_or_undefined`.

Deviasi: `plan/deviations/mz-account.md`. `audit` = `&mz_audit::AuditHandle` (lihat `crates/mz-audit/README.md`): fungsi yang menulis model ber-audit mencatat `auditLog` setara middleware Prisma Node.
