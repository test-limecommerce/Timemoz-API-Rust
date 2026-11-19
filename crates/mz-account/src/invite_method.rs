//! Port `src/users/invite-method.ts` — nilai `userCompany.inviteMethod` (VARCHAR).

/// `INVITE_METHOD.EMAIL`: diundang admin lewat `POST /users/invite`.
pub const EMAIL: &str = "EMAIL";
/// `INVITE_METHOD.LINK`: join mandiri lewat invite code.
pub const LINK: &str = "LINK";

/// `toInviteMethod`: hanya `EMAIL`/`LINK` yang lolos, selain itu `None`.
pub fn to_invite_method(value: Option<&str>) -> Option<&'static str> {
    match value {
        Some(EMAIL) => Some(EMAIL),
        Some(LINK) => Some(LINK),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist() {
        assert_eq!(to_invite_method(Some("EMAIL")), Some("EMAIL"));
        assert_eq!(to_invite_method(Some("LINK")), Some("LINK"));
        assert_eq!(to_invite_method(Some("email")), None);
        assert_eq!(to_invite_method(None), None);
    }
}
