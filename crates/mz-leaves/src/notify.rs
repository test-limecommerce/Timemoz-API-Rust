//! Email cuti fire-and-forget (`void (async () => …)().catch(() => undefined)`): dijalankan
//! lewat `mz_core::bg::spawn` SESUDAH commit; kegagalan tidak menggagalkan respons.

use mz_core::AppState;
use mz_leave::day_count::to_date_key;
use mz_leave::mail::{LeaveDecision, LeaveMailDetails};

use crate::model::RequestRow;

/// `LeaveMailDetails` dari baris pengajuan (`leaveTypeConfig?.name ?? leaveType ?? 'Cuti'`).
pub fn details(r: &RequestRow, leave_type_name: String) -> LeaveMailDetails {
    LeaveMailDetails {
        employee_name: r.u_name.clone(),
        leave_type_name,
        start_date: to_date_key(r.start_date),
        end_date: to_date_key(r.end_date),
        start_day_type: r.start_day_type.clone(),
        end_day_type: r.end_day_type.clone(),
        total_days: r.total_days,
        reason: r.reason.clone(),
    }
}

/// `notifyDecision(request, reviewerId, outcome)` (`leaves.service.ts:102`): nama reviewer
/// dicari di latar (`'approver'` bila tidak ada), lalu email keputusan ke pengaju.
pub fn decision(state: &AppState, r: &RequestRow, reviewer_id: i32, outcome: LeaveDecision) {
    let state = state.clone();
    let details = details(r, r.type_name_or("Cuti"));
    let email = r.u_username.clone();
    let uuid = r.uuid.clone();
    mz_core::bg::spawn(async move {
        let reviewer: Option<String> =
            match sqlx::query_scalar(r#"SELECT name FROM users WHERE id = $1"#)
                .bind(reviewer_id)
                .fetch_optional(&state.db)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = %e, "leave decision mail: reviewer lookup failed");
                    return;
                }
            };
        let reviewer = reviewer.unwrap_or_else(|| "approver".to_owned());
        mz_leave::mail::send_leave_decision_to_requester(
            &state, &details, &email, &reviewer, &uuid, &outcome,
        )
        .await;
    });
}

/// `sendLeaveRequestToApprover` sesudah pengajuan (`mobile-leaves.service.ts:295`).
pub fn request_to_approver(
    state: &AppState,
    details: LeaveMailDetails,
    approver_email: String,
    approver_name: String,
) {
    let state = state.clone();
    mz_core::bg::spawn(async move {
        mz_leave::mail::send_leave_request_to_approver(
            &state,
            &details,
            &approver_email,
            &approver_name,
            false,
        )
        .await;
    });
}
