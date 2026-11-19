//! `renewPayment` (`PUT /invoices/renew/:uuid`).

use chrono::Utc;
use mz_core::jsfmt::num;
use mz_core::time::now_naive;
use mz_core::{ApiResult, AppState};
use mz_paypal::{PaypalClient, PaypalError, get_approval_url};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::db::{V, invoice_status, sub_status, update_by_id};
use crate::load::{self, SubRow, TierRow, UserRow};
use crate::midtrans::Midtrans;
use crate::paypal::{invoice_urls, str_field};
use crate::util::{Fail, add_days, assert_not_manual, dmy, finish, type_error};

#[derive(sqlx::FromRow)]
struct InvoiceRow {
    id: i32,
    #[sqlx(rename = "invoiceNumber")]
    invoice_number: String,
    #[sqlx(rename = "totalPrice")]
    total_price: f64,
    #[sqlx(rename = "subscriptionId")]
    subscription_id: i32,
}

/// Node: invoices.service.ts:225 (`renewPayment`).
pub async fn renew(state: &AppState, user_id: i32, uuid: &Uuid) -> ApiResult<Value> {
    finish(
        run(state, user_id, uuid).await,
        "Failed to renew payment",
        "renewPayment",
    )
}

/// Galat di blok `try` PayPal bagian dalam: hanya status 404 yang dibedakan.
struct BranchErr {
    not_found: bool,
    detail: String,
}

impl From<PaypalError> for BranchErr {
    fn from(e: PaypalError) -> Self {
        Self {
            not_found: e.status == Some(404),
            detail: e.message,
        }
    }
}

impl From<Fail> for BranchErr {
    fn from(e: Fail) -> Self {
        // `Fail::Message` dari PayPal sudah kehilangan status; 404 PayPal dibawa lewat `From<PaypalError>`.
        Self {
            not_found: false,
            detail: format!("{e:?}"),
        }
    }
}

impl From<sqlx::Error> for BranchErr {
    fn from(e: sqlx::Error) -> Self {
        Self {
            not_found: false,
            detail: e.to_string(),
        }
    }
}

struct Ctx<'a> {
    state: &'a AppState,
    client: PaypalClient,
    user: &'a UserRow,
    invoice: &'a InvoiceRow,
    sub: &'a SubRow,
    tier: Option<&'a TierRow>,
}

async fn run(state: &AppState, user_id: i32, uuid: &Uuid) -> Result<Value, Fail> {
    let Some(user) = load::user(state, user_id).await? else {
        return Ok(json!({ "success": false, "message": "User not found", "data": null }));
    };

    let invoice = sqlx::query_as::<_, InvoiceRow>(
        r#"SELECT id, "invoiceNumber", "totalPrice", "subscriptionId" FROM invoices WHERE uuid = $1 LIMIT 1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db);
    let requester = mz_files::load_file_requester(&state.db, user_id);
    let (invoice, requester) = tokio::try_join!(invoice, requester)?;
    let sub = match &invoice {
        Some(i) => sqlx::query_as::<_, SubRow>(
            r#"SELECT s.id, s."companyId", s."pricingTierId", s."startDate", s."endDate",
                      s.status::text AS status, s.quantity, s."pricePerUser", s."paypalSubscriptionId",
                      s."paypalPlanId", s."autoRenew", s."pendingPaypalSubscriptionId"
                 FROM subscriptions s WHERE s.id = $1"#,
        )
        .bind(i.subscription_id)
        .fetch_optional(&state.db)
        .await?,
        None => None,
    };
    // Invoice company lain = tidak ada (F6.2), supaya uuid-nya tidak bocor.
    let (Some(invoice), Some(sub)) = (invoice, sub) else {
        return Ok(json!({ "success": false, "message": "Invoice not found", "data": null }));
    };
    if !mz_billing::can_read_invoice_pdf(&requester, Some(sub.company_id)) {
        return Ok(json!({ "success": false, "message": "Invoice not found", "data": null }));
    }

    // Company pemilik invoice, bukan company pemanggil (SYSTEM_ADMIN lintas company).
    assert_not_manual(state, sub.company_id).await?;

    let tier = load::tier_by_id(state, sub.pricing_tier_id).await?;
    let ctx = Ctx {
        state,
        client: PaypalClient::from_state(state),
        user: &user,
        invoice: &invoice,
        sub: &sub,
        tier: tier.as_ref(),
    };

    if let Some(pp_id) = sub.paypal_id() {
        return match paypal_branch(&ctx, pp_id).await {
            Ok(v) => Ok(v),
            // PayPal 404 (langganan sudah tidak ada) = CANCELLED → buat baru. Galat di sini lolos
            // ke `catch` luar.
            Err(e) if e.not_found => recreate(&ctx).await.map_err(|e| match e {
                RecreateErr::Fail(f) => f,
                RecreateErr::Paypal(p) => Fail::Message(p.message),
            }),
            Err(e) => {
                tracing::error!(detail = %e.detail, "PayPal subscription error");
                Ok(json!({ "success": false, "message": "Failed to process PayPal subscription" }))
            }
        };
    }

    midtrans_flow(&ctx).await
}

async fn paypal_branch(ctx: &Ctx<'_>, pp_id: &str) -> Result<Value, BranchErr> {
    let paypal_sub = ctx.client.get_subscription(pp_id).await?;
    tracing::info!(subscription = %paypal_sub, "renewPayment: PayPal subscription");
    let status = paypal_sub.get("status").cloned();
    match status.as_ref().and_then(Value::as_str) {
        Some("SUSPENDED") => {
            ctx.client
                .activate_subscription(pp_id, Some("Subscription reactivated by user"))
                .await?;
            // nextInvoiceDate = endDate - 3 hari.
            let next = add_days(ctx.sub.end_date, -3);
            let mut conn = ctx.state.db.acquire().await?;
            update_by_id(
                &mut conn,
                "subscriptions",
                ctx.sub.id,
                &[
                    ("status", sub_status("ACTIVE")),
                    ("paypalStatus", V::Str("ACTIVE".into())),
                    ("autoRenew", V::Bool(true)),
                    ("nextInvoiceDate", V::Ts(next)),
                ],
            )
            .await?;
            update_by_id(
                &mut conn,
                "invoices",
                ctx.invoice.id,
                &[("status", invoice_status("PAID"))],
            )
            .await?;
            drop(conn);
            crate::load::invalidate_subscription_lock(ctx.state, ctx.sub.company_id).await;
            Ok(json!({
                "success": true,
                "message": "Subscription reactivated successfully",
                "data": { "paypalSubscriptionId": pp_id, "status": "ACTIVE" },
            }))
        }
        Some("APPROVAL_PENDING") => {
            // Langganan ada tetapi belum disetujui — pakai ulang tautan persetujuannya.
            let approval = get_approval_url(&paypal_sub);
            if let Some(url) = &approval {
                let mut conn = ctx.state.db.acquire().await?;
                update_by_id(
                    &mut conn,
                    "invoices",
                    ctx.invoice.id,
                    &[("paymentUrl", V::Str(url.clone()))],
                )
                .await?;
            }
            Ok(json!({
                "success": true,
                "message": "Payment approval pending, please complete your payment",
                "data": {
                    "paymentUrl": approval,
                    "paypalSubscriptionId": pp_id,
                    "status": "APPROVAL_PENDING",
                },
            }))
        }
        Some("CANCELLED") => recreate(ctx).await.map_err(|e| match e {
            RecreateErr::Paypal(p) => p.into(),
            RecreateErr::Fail(f) => f.into(),
        }),
        _ => {
            let mut data = Map::new();
            data.insert("paypalSubscriptionId".into(), pp_id.into());
            // `status: undefined` dibuang `JSON.stringify`.
            if let Some(s) = status {
                data.insert("status".into(), s);
            }
            Ok(json!({
                "success": true,
                "message": "Subscription is already active",
                "data": data,
            }))
        }
    }
}

enum RecreateErr {
    Paypal(PaypalError),
    Fail(Fail),
}

impl From<sqlx::Error> for RecreateErr {
    fn from(e: sqlx::Error) -> Self {
        Self::Fail(e.into())
    }
}

impl From<Fail> for RecreateErr {
    fn from(e: Fail) -> Self {
        Self::Fail(e)
    }
}

/// Cabang `CANCELLED` / PayPal 404: langganan PayPal baru untuk invoice ini.
async fn recreate(ctx: &Ctx<'_>) -> Result<Value, RecreateErr> {
    // `user.userCompany[0].companyId` (company pemanggil).
    let Some(company_id) = mz_core::company::find_company_id(&ctx.state.db, ctx.user.id).await?
    else {
        return Err(type_error("undefined", "companyId").into());
    };
    let active = load::active_users(ctx.state, company_id).await?;
    let min_users = ctx.tier.map_or(0, |t| t.min_users);
    let quantity = active.max(if min_users == 0 {
        2
    } else {
        i64::from(min_users)
    });

    let (return_url, cancel_url) = invoice_urls(ctx.state, &ctx.invoice.invoice_number);
    let new_sub = ctx
        .client
        .create_subscription(&mz_paypal::CreateSubscriptionPayload {
            plan_id: ctx.sub.paypal_plan_id.clone().unwrap_or_default(),
            subscriber_email: ctx.user.username.clone(),
            subscriber_name: ctx.user.name.clone(),
            custom_id: Some(format!("company-{company_id}")),
            return_url,
            cancel_url,
            quantity: Some(quantity),
            start_time: None,
        })
        .await
        .map_err(RecreateErr::Paypal)?;
    let approval = get_approval_url(&new_sub);
    let new_id = str_field(&new_sub, "id");

    let mut tx = ctx.state.db.begin().await?;
    let mut sets = Vec::new();
    if let Some(id) = &new_id {
        sets.push(("paypalSubscriptionId", V::Str(id.clone())));
    }
    if let Some(st) = str_field(&new_sub, "status") {
        sets.push(("paypalStatus", V::Str(st)));
    }
    sets.push(("status", sub_status("PENDING")));
    update_by_id(&mut tx, "subscriptions", ctx.sub.id, &sets).await?;
    update_by_id(
        &mut tx,
        "invoices",
        ctx.invoice.id,
        &[
            ("paymentUrl", V::Str(approval.clone().unwrap_or_default())),
            ("status", invoice_status("UNPAID")),
        ],
    )
    .await?;
    tx.commit().await?;
    crate::load::invalidate_subscription_lock(ctx.state, ctx.sub.company_id).await;

    let mut data = Map::new();
    data.insert("paymentUrl".into(), approval.into());
    if let Some(id) = new_id {
        data.insert("paypalSubscriptionId".into(), id.into());
    }
    Ok(json!({
        "success": true,
        "message": "New subscription created, please approve payment",
        "data": data,
    }))
}

/// Alur lama Midtrans (invoice tanpa langganan PayPal).
async fn midtrans_flow(ctx: &Ctx<'_>) -> Result<Value, Fail> {
    let midtrans = Midtrans::new(ctx.state);
    midtrans
        .delete_payment_link(&ctx.invoice.invoice_number)
        .await
        .map_err(|e| Fail::Message(e.0))?;

    const INVOICE_EXPIRED_DAY: i64 = 3;
    let now = now_naive();
    let end_date = add_days(now, INVOICE_EXPIRED_DAY);
    let unix = Utc::now().timestamp();
    // `getDate()/getHours()` zona proses (produksi TZ=UTC) tetapi berlabel `+0700` — sama dengan Node.
    let start_time = format!("{} +0700", now.format("%Y-%m-%d %H:%M"));

    // `user.userCompany[0].company.name` / `.telp`.
    let company = match mz_core::company::find_company_id(&ctx.state.db, ctx.user.id).await? {
        Some(cid) => load::company(ctx.state, cid).await?,
        None => None,
    };
    let Some(company) = company else {
        return Err(type_error("undefined", "company"));
    };

    let mut payload = json!({
        "transaction_details": {
            "order_id": ctx.invoice.invoice_number,
            "gross_amount": num(ctx.invoice.total_price),
            "payment_link_id": format!("{}-renew-{unix}", ctx.invoice.invoice_number),
        },
        "expiry": { "start_time": start_time, "duration": INVOICE_EXPIRED_DAY, "unit": "days" },
        "enabled_payments": ["credit_card", "gopay", "bca_va", "bni_va"],
        "usage_limit": 1,
        "customer_details": {
            "first_name": ctx.user.name,
            "last_name": company.name,
            "email": ctx.user.username,
            "phone": company.telp,
            "notes": "",
        },
        "customer_required": false,
        "title": ctx.invoice.invoice_number,
        "payment_link_type": "FIXED_AMOUNT",
    });
    if let Some(o) = payload.as_object_mut() {
        // `webhook_key: undefined` dibuang `JSON.stringify`.
        if let Some(k) = ctx.state.config.var("MIDTRANS_WEBHOOK_KEY") {
            o.insert("webhook_key".into(), k.into());
        }
        o.insert(
            "callbacks".into(),
            json!({ "finish": format!("{}/{}", crate::util::env_js(ctx.state, "MIDTRANS_CALLBACK"), ctx.invoice.invoice_number) }),
        );
    }

    let payment = midtrans
        .create_payment_link(&payload)
        .await
        .map_err(|e| Fail::Message(e.0))?;
    let payment_url = str_field(&payment, "payment_url");

    let mut sets = Vec::new();
    if let Some(u) = &payment_url {
        sets.push(("paymentUrl", V::Str(u.clone())));
    }
    sets.push(("expiredAt", V::Ts(end_date)));
    sets.push(("status", invoice_status("UNPAID")));
    let mut conn = ctx.state.db.acquire().await?;
    update_by_id(&mut conn, "invoices", ctx.invoice.id, &sets).await?;
    drop(conn);

    let Some(tier) = ctx.tier else {
        return Err(type_error("null", "name"));
    };
    let period = format!("{} - {}", dmy(ctx.sub.start_date), dmy(ctx.sub.end_date));
    let mail = crate::mail::invoice_confirmation(crate::mail::InvoiceConfirmation {
        email: ctx.user.username.clone(),
        name: ctx.user.name.clone(),
        tier_name: tier.name.clone(),
        invoice_number: ctx.invoice.invoice_number.clone(),
        payment_url: payment_url.clone(),
        period,
        total_price: ctx.invoice.total_price,
        expired_at: dmy(ctx.sub.end_date),
    });
    // `await this.mail.sendInvoiceConfirmation(...)` (tidak pernah melempar).
    let _ = mz_core::mail::send(ctx.state, mail.as_mail()).await;

    let mut data = Map::new();
    if let Some(u) = payment_url {
        data.insert("paymentUrl".into(), u.into());
    }
    Ok(json!({ "success": true, "message": "Success renew payment", "data": data }))
}
