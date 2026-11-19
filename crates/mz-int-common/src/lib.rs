//! Fondasi bersama port `src/integrations/*` (ClickUp, Jira, Discord, Google Calendar).
//!
//! - [`company::admin_company_id`]: `resolveCompanyId` privat yang IDENTIK di
//!   `clickup.service.ts`, `jira.service.ts`, `discord.service.ts` (company tempat user
//!   ber-role admin). Google Calendar memakai `findCompanyId` biasa
//!   (`mz_core::company::find_company_id`).
//! - [`nest_jwt`]: `JwtService` modul integrasi (`JwtModule.register({ secret: SECRET_TOKEN })`):
//!   `sign(payload, { expiresIn })` dan `verify(token)` (exp DIPERIKSA, beda dengan
//!   `JwtAuthGuard`).
//! - [`api_base`]: URL dasar API eksternal yang bisa dialihkan lewat env (untuk uji
//!   integrasi dengan server tiruan; produksi memakai URL bawaan Node).
//! - [`envelope::Envelope`]: `{ success, message, data }` bertipe (urutan kunci terjaga).
//! - [`jsonb`]: kolom jsonb → `RawValue` dengan urutan kunci Postgres (= Prisma).
//! - prisma (UUID P2023) kini di `mz_core::prisma`.
//! - [`mock`]: server HTTP tiruan untuk uji integrasi (merekam request, balas JSON).

pub mod api_base;
pub mod assignee;
pub mod company;
pub mod envelope;
pub mod jsonb;
pub mod mock;
pub mod nest_jwt;
