//! `changePlan` (`POST /invoices/change-plan`) dan `changeActivePlan`
//! (`POST /invoices/change-active-plan`).

use chrono::Utc;
use mz_admin_company::common::{CompanyChange, after_company_change};
use mz_admin_company::plan_limits::{Bind, reapply_plan_override_after_tier_sync, update_company};
use mz_audit::AuditHandle;
use mz_core::jsfmt::{num, num_string};
use mz_core::time::now_naive;
use mz_core::{ApiResult, AppState};
use mz_paypal::{PaypalClient, get_approval_url};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::db::{
    NewInvoice, V, cancel_unpaid_invoices, create_invoice, create_item, invoice_status,
    paypal_source, sub_status, update_by_id,
};
use crate::load::{self, TierRow};
use crate::paypal::{NewSub, activate_free, create_subscription, invoice_urls, str_field};
use crate::util::{
    Fail, add_days, add_year_js, assert_not_manual, date_json, dmy, fail_msg, finish, iso,
    type_error,
};

/// `Math.max(activeUsers, tier.minUsers || 2)`.
fn quantity(active_users: i64, min_users: i32) -> i64 {
    active_users.max(if min_users == 0 {
        2
    } else {
        i64::from(min_users)
    })
}

/// `/month (billed yearly)` untuk paket tahunan.
fn yearly_suffix(interval: &str) -> &'static str {
    if interval == "YEAR" {
        "/month (billed yearly)"
    } else {
        ""
    }
}

fn interval_label(interval: &str) -> &'static str {
    if interval == "YEAR" {
        "Yearly"
    } else {
        "Monthly"
    }
}

fn multiplier(interval: &str) -> f64 {
    if interval == "YEAR" { 12.0 } else { 1.0 }
}

/// `prisma.company.update({ data: { ...limit tier, reportExportsUsedThisMonth: 0, reportExportsResetAt } })`
/// (audit `company`) lalu `reapplyPlanOverrideAfterTierSync`.
async fn sync_company_limits(
    conn: &mut sqlx::PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    t: &TierRow,
) -> Result<(), Fail> {
    let sets = vec![
        ("maxUsers".to_owned(), Bind::I32(t.max_users)),
        ("maxProject".to_owned(), Bind::I32(t.max_project)),
        (
            "screenshotResolution".to_owned(),
            Bind::Str(t.screenshot_resolution.clone()),
        ),
        (
            "screenshotRetentionDays".to_owned(),
            Bind::I32(t.screenshot_retention_days),
        ),
        (
            "reportExportsPerMonth".to_owned(),
            Bind::I32(t.report_exports_per_month),
        ),
        // Reset counter on plan change.
        ("reportExportsUsedThisMonth".to_owned(), Bind::I32(0)),
        ("reportExportsResetAt".to_owned(), Bind::Ts(now_naive())),
        (
            "dataHistoryMonths".to_owned(),
            Bind::I32(t.data_history_months),
        ),
        (
            "budgetAlertDeliveryChannel".to_owned(),
            Bind::Channel(t.budget_alert_delivery_channel.clone()),
        ),
        (
            "clientProofExportAllowed".to_owned(),
            Bind::Bool(t.client_proof_export_allowed),
        ),
    ];
    update_company(conn, audit, company_id, &sets).await?;
    Ok(())
}

/// Sesudah commit: override paket per company (spec 007) menang atas batas tier yang baru
/// disalin; cache Rust (user-me, subscription-lock).
async fn after_plan_change(state: &AppState, audit: &AuditHandle, company_id: i32) {
    reapply_plan_override_after_tier_sync(state, audit, company_id).await;
    after_company_change(
        state,
        company_id,
        CompanyChange {
            modules: false,
            divisions: false,
            plan: true,
            user_me: true,
        },
    )
    .await;
    load::invalidate_subscription_lock(state, company_id).await;
}

/// Node: invoices.service.ts:562 (`changePlan`).
pub async fn change_plan(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    new_pricing_uuid: &Uuid,
) -> ApiResult<Value> {
    finish(
        change_plan_run(state, audit, user_id, new_pricing_uuid).await,
        "Failed to change plan",
        "changePlan",
    )
}

async fn change_plan_run(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    new_pricing_uuid: &Uuid,
) -> Result<Value, Fail> {
    let Some(ctx) = load::context(state, user_id).await? else {
        return Ok(fail_msg("User not found"));
    };
    let Some(company) = ctx.company else {
        return Ok(fail_msg("Company not found"));
    };
    // Langganan manual dikelola tim Timemoz (spec 007 R-13).
    assert_not_manual(state, company.id).await?;
    let Some(sw) = ctx.subscription else {
        return Ok(fail_msg("No active subscription found"));
    };
    let sub = sw.sub;

    let Some(new_tier) = load::tier_by_uuid(state, new_pricing_uuid, true).await? else {
        return Ok(fail_msg("Pricing tier not found"));
    };
    if new_tier.price_per_user_usd == 0.0 {
        if new_tier.id == sub.pricing_tier_id {
            return Ok(fail_msg("New plan is the same as the current plan"));
        }
        return activate_free(state, audit, &sub, new_tier.id, company.id).await;
    }
    let Some(plan_id) = new_tier.plan_id().map(str::to_owned) else {
        return Ok(fail_msg(
            "New pricing tier does not have a PayPal plan configured",
        ));
    };
    if new_tier.id == sub.pricing_tier_id {
        return Ok(fail_msg("New plan is the same as the current plan"));
    }

    // Dari langganan gratis, upgrade WAJIB lewat jalur pending (`changeActivePlan`): tier/harga/
    // tanggal baru hanya ditulis webhook setelah pembayaran.
    if sub.price_per_user <= 0.0 {
        return change_active_plan(state, audit, user_id, new_pricing_uuid)
            .await
            .map_err(Fail::Throw);
    }

    let company_id = company.id;
    let active = load::active_users(state, company_id).await?;
    let quantity = quantity(active, new_tier.min_users);
    let price = new_tier.price_per_user_usd;
    let interval = new_tier.interval().to_owned();
    let mult = multiplier(&interval);
    let sub_price = quantity as f64 * price * mult;
    let tax_price = 0.0;
    let total_price = sub_price + tax_price;
    let invoice_number = mz_billing::generate_invoice_number(Utc::now());
    let (return_url, cancel_url) = invoice_urls(state, &invoice_number);
    let client = PaypalClient::from_state(state);

    if company.is_trial {
        // === TRIAL FLOW === mulai hari ini, berakhir sesuai interval (bayar di muka).
        let start = now_naive();
        let end = if interval == "YEAR" {
            add_year_js(start)
        } else {
            add_days(start, 30)
        };

        // Batalkan invoice UNPAID lama (sebelum PayPal, seperti Node).
        {
            let mut conn = state.db.acquire().await?;
            cancel_unpaid_invoices(&mut conn, sub.id).await?;
        }

        // Langganan PayPal baru (tanpa startTime = ditagih saat disetujui).
        let new_sub = create_subscription(
            &client,
            NewSub {
                plan_id: &plan_id,
                user: &ctx.user,
                company_id,
                return_url,
                cancel_url,
                quantity,
                start_time: None,
            },
        )
        .await?;
        let approval = get_approval_url(&new_sub);
        let new_id = str_field(&new_sub, "id");

        let mut tx = state.db.begin().await?;
        let mut sets = vec![("pricingTierId", V::I32(new_tier.id))];
        if let Some(id) = &new_id {
            sets.push(("paypalSubscriptionId", V::Str(id.clone())));
        }
        sets.push(("paypalPlanId", V::Str(plan_id.clone())));
        if let Some(st) = str_field(&new_sub, "status") {
            sets.push(("paypalStatus", V::Str(st)));
        }
        sets.extend([
            ("pricePerUser", V::F64(price)),
            ("billingInterval", V::Str(interval.clone())),
            ("status", sub_status("PENDING")),
            ("billingSource", paypal_source()),
            ("startDate", V::Ts(start)),
            ("endDate", V::Ts(end)),
        ]);
        update_by_id(&mut tx, "subscriptions", sub.id, &sets).await?;

        let invoice_id = create_invoice(
            &mut tx,
            &NewInvoice {
                subscription_id: sub.id,
                invoice_number: &invoice_number,
                payment_url: approval.as_deref().unwrap_or_default(),
                sub_price,
                tax_price,
                total_price,
                expired_at: add_days(now_naive(), 3),
                status: None,
            },
        )
        .await?;
        let description = format!(
            "{} Subscription - {quantity} user(s) @ ${}/user{}",
            interval_label(&interval),
            num_string(price),
            yearly_suffix(&interval)
        );
        create_item(
            &mut tx,
            invoice_id,
            &description,
            quantity as f64,
            price * mult,
            sub_price,
        )
        .await?;

        // `isUpgradingToPro` (`screenshotRetentionDays === null`) tak pernah benar: kolomnya
        // NOT NULL, jadi `cleanupCompressedScreenshots` tidak pernah terpanggil di sini (sama Node).

        // Batas penggunaan dari tier baru (pratinjau selama PENDING). `isTrial` tetap true —
        // hanya diubah `handlePaymentCompleted` setelah pembayaran.
        sync_company_limits(&mut tx, audit, company_id, &new_tier).await?;
        tx.commit().await?;
        after_plan_change(state, audit, company_id).await;

        let mut data = Map::new();
        data.insert("paymentUrl".into(), approval.into());
        if let Some(id) = new_id {
            data.insert("paypalSubscriptionId".into(), id.into());
        }
        data.insert("invoiceNumber".into(), invoice_number.into());
        data.insert("totalPrice".into(), num(total_price));
        data.insert("startDate".into(), date_json(start));
        data.insert("endDate".into(), date_json(end));
        return Ok(json!({
            "success": true,
            "message": "Plan changed successfully. Please approve the new subscription.",
            "data": data,
        }));
    }

    // === ACTIVE USER FLOW ===
    if let Some(old) = sub.paypal_id() {
        client
            .cancel_subscription(old, Some("Plan changed by user"))
            .await?;
    }
    let new_sub = create_subscription(
        &client,
        NewSub {
            plan_id: &plan_id,
            user: &ctx.user,
            company_id,
            return_url,
            cancel_url,
            quantity,
            start_time: None,
        },
    )
    .await?;
    let approval = get_approval_url(&new_sub);
    let new_id = str_field(&new_sub, "id");

    let mut tx = state.db.begin().await?;
    let mut sets = vec![("pricingTierId", V::I32(new_tier.id))];
    if let Some(id) = &new_id {
        sets.push(("paypalSubscriptionId", V::Str(id.clone())));
    }
    sets.push(("paypalPlanId", V::Str(plan_id.clone())));
    if let Some(st) = str_field(&new_sub, "status") {
        sets.push(("paypalStatus", V::Str(st)));
    }
    sets.extend([
        ("pricePerUser", V::F64(price)),
        ("billingInterval", V::Str(interval.clone())),
        ("status", sub_status("PENDING")),
        ("billingSource", paypal_source()),
    ]);
    update_by_id(&mut tx, "subscriptions", sub.id, &sets).await?;

    // Invoice terbaru langganan ini ikut diperbarui.
    let existing: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT id, "invoiceNumber" FROM invoices WHERE "subscriptionId" = $1
            ORDER BY "createdAt" DESC, id DESC LIMIT 1"#,
    )
    .bind(sub.id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((inv_id, _)) = &existing {
        update_by_id(
            &mut tx,
            "invoices",
            *inv_id,
            &[
                ("paymentUrl", V::Str(approval.clone().unwrap_or_default())),
                ("subPrice", V::F64(sub_price)),
                ("taxPrice", V::F64(tax_price)),
                ("totalPrice", V::F64(total_price)),
                ("status", invoice_status("UNPAID")),
            ],
        )
        .await?;
    }
    sync_company_limits(&mut tx, audit, company_id, &new_tier).await?;
    tx.commit().await?;
    after_plan_change(state, audit, company_id).await;

    let mut data = Map::new();
    data.insert("paymentUrl".into(), approval.into());
    if let Some(id) = new_id {
        data.insert("paypalSubscriptionId".into(), id.into());
    }
    data.insert(
        "invoiceNumber".into(),
        existing
            .map(|(_, n)| n)
            .filter(|n| !n.is_empty())
            .unwrap_or(invoice_number)
            .into(),
    );
    data.insert("totalPrice".into(), num(total_price));
    Ok(json!({
        "success": true,
        "message": "Plan changed successfully. Please approve the new subscription.",
        "data": data,
    }))
}

/// Urutan tier untuk deteksi upgrade/downgrade (`TIER_LEVEL_ORDER`); tier lain jatuh ke `id`.
fn tier_level(name: &str, id: i32) -> i32 {
    match name {
        "Starter" => 1,
        "Grow" => 2,
        _ => id,
    }
}

/// Node: invoices.service.ts:881 (`changeActivePlan`). Upgrade: invoice UNPAID + langganan PayPal
/// pending (ditagih saat disetujui; langganan lama dibatalkan webhook sesudah bayar). Downgrade:
/// langganan lama dibatalkan sekarang, yang baru mulai (dan ditagih PayPal) di `endDate`.
pub async fn change_active_plan(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    new_pricing_uuid: &Uuid,
) -> ApiResult<Value> {
    finish(
        change_active_plan_run(state, audit, user_id, new_pricing_uuid).await,
        "Failed to change active plan",
        "changeActivePlan",
    )
}

async fn change_active_plan_run(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    new_pricing_uuid: &Uuid,
) -> Result<Value, Fail> {
    let Some(ctx) = load::context(state, user_id).await? else {
        return Ok(fail_msg("User not found"));
    };
    let Some(company) = ctx.company else {
        return Ok(fail_msg("Company not found"));
    };
    assert_not_manual(state, company.id).await?;

    // Langganan gratis boleh upgrade walau tidak ACTIVE (INACTIVE/TRIAL) — satu-satunya jalan keluar
    // dari kunci SUBSCRIPTION_EXPIRED; statusnya baru ACTIVE di webhook pembayaran.
    let is_free_upgrade = ctx.subscription.as_ref().is_some_and(|s| {
        (s.sub.status == "INACTIVE" || s.sub.status == "TRIAL") && s.sub.price_per_user <= 0.0
    });
    let Some(sw) = ctx
        .subscription
        .filter(|s| s.sub.status == "ACTIVE" || is_free_upgrade)
    else {
        return Ok(fail_msg("No active subscription found"));
    };
    let sub = sw.sub;

    let Some(new_tier) = load::tier_by_uuid(state, new_pricing_uuid, true).await? else {
        return Ok(fail_msg("Pricing tier not found"));
    };
    if new_tier.price_per_user_usd == 0.0 {
        if new_tier.id == sub.pricing_tier_id {
            return Ok(fail_msg("New plan is the same as the current plan"));
        }
        return activate_free(state, audit, &sub, new_tier.id, company.id).await;
    }
    if new_tier.id == sub.pricing_tier_id {
        return Ok(fail_msg("New plan is the same as the current plan"));
    }
    let Some(plan_id) = new_tier.plan_id().map(str::to_owned) else {
        return Ok(fail_msg(
            "New pricing tier does not have a PayPal plan configured",
        ));
    };

    let company_id = company.id;
    let active = load::active_users(state, company_id).await?;
    let quantity = quantity(active, new_tier.min_users);
    let invoice_number = mz_billing::generate_invoice_number(Utc::now());
    let interval = new_tier.interval().to_owned();
    let mult = multiplier(&interval);
    let price = new_tier.price_per_user_usd;
    let unit_price = price * mult;
    let sub_price = unit_price * quantity as f64;

    // `subscription.pricingTier.name` (relasi null → TypeError).
    let Some(current) = sw.tier else {
        return Err(type_error("null", "name"));
    };
    let current_level = tier_level(&current.name, current.id);
    let new_level = tier_level(&new_tier.name, new_tier.id);
    // Dari paket gratis selalu upgrade.
    let is_downgrade = sub.price_per_user > 0.0 && new_level < current_level;

    let client = PaypalClient::from_state(state);
    // Bersihkan langganan pending dari percobaan sebelumnya.
    if let Some(pending) = sub
        .pending_paypal_subscription_id
        .as_deref()
        .filter(|s| !s.is_empty())
        && let Err(err) = client
            .cancel_subscription(pending, Some("Replaced by new plan change request"))
            .await
    {
        tracing::warn!("Failed to cancel previous pending sub: {err}");
    }

    let (return_url, cancel_url) = invoice_urls(state, &invoice_number);

    if is_downgrade {
        // === DOWNGRADE FLOW === langganan lama dibatalkan sekarang (cegah tagihan ganda di
        // endDate); yang baru mulai & ditagih PayPal di endDate.
        if let Some(old) = sub.paypal_id()
            && let Err(err) = client
                .cancel_subscription(
                    old,
                    Some("Downgrade — old plan cancelled, new plan scheduled at cycle end"),
                )
                .await
        {
            tracing::warn!("Failed to cancel old PayPal sub on downgrade: {err}");
        }
        let new_sub = create_subscription(
            &client,
            NewSub {
                plan_id: &plan_id,
                user: &ctx.user,
                company_id,
                return_url,
                cancel_url,
                quantity,
                start_time: Some(iso(sub.end_date)),
            },
        )
        .await?;
        let approval = get_approval_url(&new_sub).unwrap_or_default();
        let new_id = str_field(&new_sub, "id");

        let mut tx = state.db.begin().await?;
        // Invoice UNPAID basi (mis. scheduler D-3) dibatalkan supaya tidak ditandai PAID oleh webhook.
        cancel_unpaid_invoices(&mut tx, sub.id).await?;
        let mut sets = vec![("pendingPricingTierId", V::I32(new_tier.id))];
        if let Some(id) = &new_id {
            sets.push(("pendingPaypalSubscriptionId", V::Str(id.clone())));
        }
        sets.extend([
            ("pendingPaypalPlanId", V::Str(plan_id.clone())),
            ("nextInvoiceDate", V::OptTs(None)),
            ("paypalStatus", V::Str("CANCELLED".into())),
        ]);
        update_by_id(&mut tx, "subscriptions", sub.id, &sets).await?;
        tx.commit().await?;
        load::invalidate_subscription_lock(state, company_id).await;

        let mut data = Map::new();
        data.insert("approvalUrl".into(), approval.into());
        if let Some(id) = new_id {
            data.insert("paypalSubscriptionId".into(), id.into());
        }
        data.insert("currentPlan".into(), current.name.clone().into());
        data.insert("newPlan".into(), new_tier.name.clone().into());
        data.insert("effectiveDate".into(), date_json(sub.end_date));
        return Ok(json!({
            "success": true,
            "message": format!(
                "Downgrade scheduled. Current plan stays active until {}, then switches to {}.",
                dmy(sub.end_date),
                new_tier.name
            ),
            "data": data,
        }));
    }

    // === UPGRADE FLOW === ditagih saat disetujui; langganan lama dibatalkan webhook sesudah bayar.
    let new_sub = create_subscription(
        &client,
        NewSub {
            plan_id: &plan_id,
            user: &ctx.user,
            company_id,
            return_url,
            cancel_url,
            quantity,
            start_time: None,
        },
    )
    .await?;
    let approval = get_approval_url(&new_sub).unwrap_or_default();
    let new_id = str_field(&new_sub, "id");

    let mut tx = state.db.begin().await?;
    let mut sets = vec![("pendingPricingTierId", V::I32(new_tier.id))];
    if let Some(id) = &new_id {
        sets.push(("pendingPaypalSubscriptionId", V::Str(id.clone())));
    }
    sets.extend([
        ("pendingPaypalPlanId", V::Str(plan_id.clone())),
        // Cegah tagihan ganda dari scheduler.
        ("nextInvoiceDate", V::OptTs(None)),
        ("billingSource", paypal_source()),
    ]);
    update_by_id(&mut tx, "subscriptions", sub.id, &sets).await?;

    // Invoice UNPAID — ditandai PAID webhook sesudah pembayaran.
    let invoice_id = create_invoice(
        &mut tx,
        &NewInvoice {
            subscription_id: sub.id,
            invoice_number: &invoice_number,
            payment_url: &approval,
            sub_price,
            tax_price: 0.0,
            total_price: sub_price,
            expired_at: add_days(now_naive(), 3),
            status: Some("UNPAID"),
        },
    )
    .await?;
    let description = format!(
        "{} Subscription (upgrade to {}) - {quantity} user(s) @ ${}/user{}",
        interval_label(&interval),
        new_tier.name,
        num_string(price),
        yearly_suffix(&interval)
    );
    create_item(
        &mut tx,
        invoice_id,
        &description,
        quantity as f64,
        unit_price,
        sub_price,
    )
    .await?;
    tx.commit().await?;
    load::invalidate_subscription_lock(state, company_id).await;

    let mut data = Map::new();
    data.insert("approvalUrl".into(), approval.into());
    if let Some(id) = new_id {
        data.insert("paypalSubscriptionId".into(), id.into());
    }
    data.insert("invoiceNumber".into(), invoice_number.into());
    data.insert("currentPlan".into(), current.name.clone().into());
    data.insert("newPlan".into(), new_tier.name.clone().into());
    data.insert("totalPrice".into(), num(sub_price));
    Ok(json!({
        "success": true,
        "message": "Upgrade initiated. Please complete payment to activate the new plan.",
        "data": data,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kuantitas_dan_level() {
        assert_eq!(quantity(1, 0), 2);
        assert_eq!(quantity(1, 5), 5);
        assert_eq!(quantity(9, 5), 9);
        assert_eq!(tier_level("Starter", 99), 1);
        assert_eq!(tier_level("Grow", 99), 2);
        assert_eq!(tier_level("Pro", 7), 7);
        assert_eq!(multiplier("YEAR"), 12.0);
        assert_eq!(interval_label("MONTH"), "Monthly");
        assert_eq!(yearly_suffix("YEAR"), "/month (billed yearly)");
        // Deskripsi item: angka JS (`$4.5`, `$10`).
        assert_eq!(num_string(4.5), "4.5");
        assert_eq!(num_string(10.0), "10");
    }
}
