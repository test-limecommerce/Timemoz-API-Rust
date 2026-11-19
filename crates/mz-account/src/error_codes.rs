//! `src/auth/auth-error-codes.ts` — kode galat yang dibaca klien di field `code`.

pub const INVALID_CREDENTIALS: &str = "INVALID_CREDENTIALS";
pub const ACCOUNT_NOT_ACTIVE: &str = "ACCOUNT_NOT_ACTIVE";
pub const ACCOUNT_DEACTIVATED: &str = "ACCOUNT_DEACTIVATED";
pub const SUBSCRIPTION_INACTIVE: &str = "SUBSCRIPTION_INACTIVE";
pub const NO_ACTIVE_COMPANY: &str = "NO_ACTIVE_COMPANY";
pub const INVITE_INVALID: &str = "INVITE_INVALID";
pub const EMAIL_TAKEN: &str = "EMAIL_TAKEN";
pub const ALREADY_MEMBER: &str = "ALREADY_MEMBER";
pub const COMPANY_FULL: &str = "COMPANY_FULL";
pub const CONFIRM_LINK_INVALID: &str = "CONFIRM_LINK_INVALID";
pub const CONFIRM_WRONG_PASSWORD: &str = "CONFIRM_WRONG_PASSWORD";
