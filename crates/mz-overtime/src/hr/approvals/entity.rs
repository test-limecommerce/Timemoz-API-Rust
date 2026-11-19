//! Bentuk respons route approvals (urutan kunci = entity Node):
//! `entities/approval-row.entity.ts`, `approval-detail.entity.ts`, `approval-quota.entity.ts`,
//! `overtime-order-result.entity.ts`, `OvertimePaginationMetaEntity`.

use serde::Serialize;

use crate::hr::common::mapper::OvertimeRequestEntity;
use crate::math::QuotaEvaluation;

/// `{ data }`.
#[derive(Debug, Clone, Serialize)]
pub struct Data<T: Serialize> {
    pub data: T,
}

/// `OvertimePaginationMetaEntity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginationMeta {
    pub total: i64,
    pub page: i64,
    pub limit: i64,
    pub total_pages: i64,
}

/// `OvertimeApprovalRowEntity` = `OvertimeRequestEntity` + `quota` + `canDecide`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRow {
    #[serde(flatten)]
    pub request: OvertimeRequestEntity,
    pub quota: QuotaEvaluation,
    pub can_decide: bool,
}

/// `OvertimeApprovalListResult`: `{ data, meta, readOnly }`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalList {
    pub data: Vec<ApprovalRow>,
    pub meta: PaginationMeta,
    pub read_only: bool,
}

/// `OvertimeApprovalDetailEntity` = `OvertimeRequestEntity` + `canDecide`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDetail {
    #[serde(flatten)]
    pub request: OvertimeRequestEntity,
    pub can_decide: bool,
}

/// `OvertimeOrderRefusalCode` (`OVERTIME_ORDER_REFUSAL_CODES`).
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum OrderRefusalCode {
    OVERTIME_DATE_TAKEN,
    OVERTIME_EMPLOYEE_ON_LEAVE,
    OVERTIME_DURATION_EXCEEDS_DAY,
    OVERTIME_EMPLOYEE_NOT_ELIGIBLE,
    OVERTIME_ORDER_SELF,
    ROLE_TOO_HIGH,
}

impl OrderRefusalCode {
    /// `ORDER_REFUSAL_MESSAGE`.
    pub fn message(self) -> &'static str {
        match self {
            Self::OVERTIME_DATE_TAKEN => {
                "Karyawan sudah punya pengajuan lembur aktif di tanggal ini."
            }
            Self::OVERTIME_EMPLOYEE_ON_LEAVE => "Karyawan cuti sehari penuh di tanggal ini.",
            Self::OVERTIME_EMPLOYEE_NOT_ELIGIBLE => {
                "Karyawan tidak dapat diberi lembur di perusahaan ini."
            }
            Self::OVERTIME_DURATION_EXCEEDS_DAY => {
                "Lembur + jam normal karyawan di tanggal ini melebihi 24 jam."
            }
            Self::OVERTIME_ORDER_SELF => "Anda tidak dapat memerintah lembur untuk diri sendiri.",
            Self::ROLE_TOO_HIGH => "Jabatan karyawan lebih tinggi dari jabatan Anda.",
        }
    }
}

/// `OvertimeOrderRefusalEntity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRefusal {
    pub user_id: String,
    pub code: OrderRefusalCode,
    pub message: &'static str,
}

/// `OvertimeOrderResultEntity`.
#[derive(Debug, Clone, Serialize)]
pub struct OrderResult {
    pub created: Vec<OvertimeRequestEntity>,
    pub refused: Vec<OrderRefusal>,
}

/// `OvertimeApprovalQuotaEntryEntity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaEntry {
    pub user_id: String,
    pub quota: Option<QuotaEvaluation>,
    pub refusal: Option<OrderRefusalCode>,
}
