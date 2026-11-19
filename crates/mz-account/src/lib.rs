//! Helper domain akun yang dipakai BERSAMA oleh `mz-auth` dan `mz-users`.
//!
//! Tiap modul adalah port satu berkas Node (lihat `README.md` untuk pemetaan).
//! Konvensi tanda tangan:
//! - fungsi yang menyentuh banyak tabel / butuh cache / mail menerima `&AppState`;
//! - fungsi yang HARUS berjalan di dalam transaksi pemanggil menerima
//!   `&mut PgConnection` (`&mut *tx`); pemanggil tanpa transaksi cukup
//!   `state.db.acquire().await?`;
//! - galat berbentuk [`mz_core::ApiError`] dengan status/body/pesan yang sama
//!   dengan Node.

pub mod anti_lockout;
pub mod assignable_role;
pub mod company_response;
pub mod customer_types;
pub mod division_membership;
pub mod error_codes;
pub mod find_one;
pub mod invite_method;
pub mod invite_token;
pub mod mail;
pub mod manual_seat_limit;
pub mod new_employee_default;
pub mod password;
pub mod plan_boost;
pub mod random_password;
pub mod role_summary;
pub mod signup_tracking;
pub mod target_user;
pub mod uoms;
pub mod user_ops;
pub mod username_conflict;
pub mod util;
pub mod visit_masters;
