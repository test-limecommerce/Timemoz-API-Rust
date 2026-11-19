//! Port `src/permissions/feature.registry.ts` + `src/permissions/role-templates.ts`.
//!
//! `FEATURES` adalah SATU sumber "fitur apa saja yang bisa dicentang"; `key` tersimpan di
//! `rolePermission.featureKey`. Urutan field struct = urutan kunci objek literal Node (dijaga
//! test golden di `mz-roles` (`tests/golden/features.json`, hasil `mz-roles/tools/dump_node_registry.js`),
//! jadi `GET /v1/admin/permissions/features` byte-identik dengan `JSON.stringify(FEATURES)`.
//!
//! Satu sumber untuk `mz-roles` dan gerbang izin: `mz_core::permissions::LEGACY_OPEN_FEATURES`
//! diturunkan dari entri `legacy_open` di sini.

use std::collections::{HashSet, VecDeque};
use std::sync::LazyLock;

use crate::enums::{BaseRole, FeatureModule, PermissionAction};
use serde::Serialize;

const READ: PermissionAction = PermissionAction::READ;
const WRITE: PermissionAction = PermissionAction::WRITE;
const DELETE: PermissionAction = PermissionAction::DELETE;

/// `FeatureDefinition` (field opsional Node = `Option`, tidak diserialisasi bila `None`).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureDefinition {
    pub key: &'static str,
    pub label: &'static str,
    pub label_en: &'static str,
    pub description: &'static str,
    pub description_en: &'static str,
    pub group: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<FeatureModule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires: Option<&'static [&'static str]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_only: Option<bool>,
    pub surfaces: &'static [&'static str],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub person_scoped: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_open: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withheld_from_built_in: Option<&'static [&'static str]>,
    pub actions: &'static [PermissionAction],
}

impl FeatureDefinition {
    pub fn is_system_only(&self) -> bool {
        self.system_only == Some(true)
    }
    pub fn is_person_scoped(&self) -> bool {
        self.person_scoped == Some(true)
    }
    pub fn is_legacy_open(&self) -> bool {
        self.legacy_open == Some(true)
    }
    pub fn has_surface(&self, surface: &str) -> bool {
        self.surfaces.contains(&surface)
    }
}

pub static FEATURES: &[FeatureDefinition] = &[
    FeatureDefinition {
        key: "users",
        label: "Karyawan",
        label_en: "Users",
        description: "Daftar karyawan, undangan, penonaktifan, tarif upah, dan batas jam kerja.",
        description_en: "Employee list, invitations, deactivation, pay rates, and work-hour limits.",
        group: "Data Management",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "projects",
        label: "Proyek",
        label_en: "Projects",
        description: "Proyek, tipe proyek, tugas di dalamnya, budget, dan laporan pemakaian budget.",
        description_en: "Projects, project types, their tasks, budgets, and budget usage reports.",
        group: "Data Management",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: Some(true),
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "roles",
        label: "Peran & Hak Akses",
        label_en: "Roles & Permissions",
        description: "Membuat dan mengubah role custom beserta menu yang boleh dibukanya.",
        description_en: "Create and edit custom roles and which menus they can open.",
        group: "Data Management",
        module: None,
        requires: Some(&["users"]),
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "clock",
        label: "Absensi",
        label_en: "Attendance",
        description: "Clock in/out, istirahat, selfie absensi, dan rekap kehadiran perusahaan.",
        description_en: "Clock in/out, breaks, attendance selfies, and the company attendance summary.",
        group: "Attendance",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: Some(true),
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "geofences",
        label: "Area Absensi",
        label_en: "Geofences",
        description: "Titik dan radius lokasi yang sah untuk absensi, beserta jadwalnya.",
        description_en: "Valid attendance locations and their radius, along with their schedule.",
        group: "Attendance",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "negative-zones",
        label: "Zona Terlarang",
        label_en: "Negative Zones",
        description: "Area yang justru tidak boleh dipakai absensi.",
        description_en: "Areas where attendance is not allowed.",
        group: "Attendance",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "kyc",
        label: "Verifikasi Wajah",
        label_en: "Face Verification",
        description: "Pendaftaran dan pemeriksaan wajah untuk absensi selfie.",
        description_en: "Face enrollment and verification for selfie attendance.",
        group: "Attendance",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "leaves",
        label: "Cuti",
        label_en: "Leave",
        description: "Pengajuan cuti, persetujuan, kalender cuti, dan laporan pemakaiannya.",
        description_en: "Leave requests, approvals, the leave calendar, and usage reports.",
        group: "Leave",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: Some(true),
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "leave-config",
        label: "Pengaturan Cuti",
        label_en: "Leave Configuration",
        description: "Jenis cuti, kebijakan dan tier, saldo karyawan, serta hari libur perusahaan.",
        description_en: "Leave types, policies and tiers, employee balances, and company holidays.",
        group: "Leave",
        module: None,
        requires: Some(&["leaves"]),
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "timesheets",
        label: "Timesheet",
        label_en: "Timesheets",
        description: "Pengiriman timesheet mingguan, persetujuan, revisi, dan ekspornya.",
        description_en: "Weekly timesheet submission, approval, revisions, and exports.",
        group: "Time Tracking",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: Some(true),
        legacy_open: None,
        withheld_from_built_in: Some(&["GUEST"]),
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "screenshots",
        label: "Screenshot",
        label_en: "Screenshots",
        description: "Tangkapan layar yang direkam aplikasi desktop saat tracking.",
        description_en: "Screenshots captured by the desktop app while tracking.",
        group: "Time Tracking",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: Some(true),
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "activity-logs",
        label: "Log Aktivitas",
        label_en: "Activity Logs",
        description: "Rekaman aktivitas aplikasi dan situs yang dipakai selama tracking.",
        description_en: "Records of apps and sites used during tracking.",
        group: "Time Tracking",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: Some(true),
        legacy_open: Some(true),
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "manual-time-approvals",
        label: "Persetujuan Waktu Manual",
        label_en: "Manual Time Approvals",
        description: "Permintaan penambahan waktu manual yang menunggu persetujuan approver.",
        description_en: "Manual time-addition requests awaiting approver sign-off.",
        group: "Time Tracking",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: Some(true),
        legacy_open: Some(true),
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "manual-time-settings",
        label: "Pengaturan Waktu Manual",
        label_en: "Manual Time Settings",
        description: "Aturan persetujuan waktu manual untuk seluruh company.",
        description_en: "Company-wide approval rules for manual time.",
        group: "Time Tracking",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: Some(&["STAFF"]),
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "divisions",
        label: "Divisi",
        label_en: "Divisions",
        description: "Kelompok karyawan dengan satu atau lebih leader yang bisa melihat data anggotanya.",
        description_en: "Employee groups with one or more leaders who can view their members' data.",
        group: "Time Tracking",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "overtime",
        label: "Lembur",
        label_en: "Overtime",
        description: "Pengajuan lembur, perintah lembur, persetujuan, dan status lembur karyawan.",
        description_en: "Overtime requests, overtime orders, approvals, and employee overtime status.",
        group: "Time Tracking",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "overtime-report",
        label: "Laporan Lembur",
        label_en: "Overtime Report",
        description: "Laporan lembur karyawan per periode beserta ekspornya.",
        description_en: "Employee overtime reports per period and their exports.",
        group: "Time Tracking",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: Some(&["STAFF"]),
        actions: &[READ],
    },
    FeatureDefinition {
        key: "overtime-settings",
        label: "Pengaturan Lembur",
        label_en: "Overtime Settings",
        description: "Mengaktifkan lembur, batas pengajuan mundur, dan daftar approver lembur.",
        description_en: "Turn overtime on or off, the back-dating window, and the overtime approver list.",
        group: "Time Tracking",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: Some(&["STAFF"]),
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "stores",
        label: "Toko",
        label_en: "Stores",
        description: "Data toko, lokasinya, QR code, dan sales yang berhak.",
        description_en: "Store data, locations, QR codes, and the sales reps assigned to them.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: None,
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "store-groups",
        label: "Wilayah Toko",
        label_en: "Store Regions",
        description: "Pengelompokan toko per wilayah dan penugasan sales ke wilayah itu.",
        description_en: "Grouping stores into regions and assigning sales reps to those regions.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["stores"]),
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "store-visits",
        label: "Kunjungan Toko",
        label_en: "Store Visits",
        description: "Check-in/check-out kunjungan sales, foto bukti, dan ekspor riwayatnya.",
        description_en: "Sales rep visit check-in/check-out, proof photos, and history exports.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["stores"]),
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: Some(true),
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "store-visit-plans",
        label: "Jadwal Kunjungan",
        label_en: "Visit Plans",
        description: "Rencana kunjungan per sales per hari, kalendernya, dan kepatuhannya.",
        description_en: "Per-rep daily visit plans, their calendar, and compliance.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["stores"]),
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "visit-masters",
        label: "Master Kunjungan",
        label_en: "Visit Master Data",
        description: "Tipe kunjungan dan frekuensi kunjungan.",
        description_en: "Visit types and visit frequencies.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["store-visit-plans"]),
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "store-visit-forms",
        label: "Form Kunjungan",
        label_en: "Visit Forms",
        description: "Template form yang diisi sales saat kunjungan, versinya, dan hasil isiannya.",
        description_en: "Form templates sales reps fill in during visits, their versions, and submitted responses.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["store-visits"]),
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "customers",
        label: "Customer",
        label_en: "Customers",
        description: "Data customer pemilik toko, tipe, group, channel, dan sub channel-nya.",
        description_en: "Customer records that own stores, with their type, group, channel, and sub channel.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "customer-masters",
        label: "Master Customer",
        label_en: "Customer Master Data",
        description: "Daftar pilihan tipe, group, channel, dan sub channel customer.",
        description_en: "Reference lists of customer types, groups, channels, and sub channels.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["customers"]),
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "products",
        label: "Produk",
        label_en: "Products",
        description: "Master produk, satuan, barcode, dan custom field per company.",
        description_en: "Product master data, units of measure, barcodes, and per-company custom fields.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: None,
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "product-masters",
        label: "Master Produk",
        label_en: "Product Master Data",
        description: "Kategori, sub kategori, brand, principal, satuan, dan pajak.",
        description_en: "Categories, sub categories, brands, principals, units, and taxes.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["products"]),
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "price-lists",
        label: "Daftar Harga",
        label_en: "Price Lists",
        description: "Daftar harga per periode dan penugasannya ke customer.",
        description_en: "Period-based price lists and their assignment to customers.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["products"]),
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "order-masters",
        label: "Data Master Order",
        label_en: "Order Master Data",
        description: "Tipe order, alasan, termin bayar, dan metode bayar.",
        description_en: "Order types, reasons, payment terms, and payment methods.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "orders",
        label: "Order",
        label_en: "Orders",
        description: "Order penjualan, sample, free goods, dan replacement, beserta custom field-nya.",
        description_en: "Sales, sample, free goods, and replacement orders, and their custom fields.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["products"]),
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "order-approvals",
        label: "Persetujuan Order",
        label_en: "Order Approvals",
        description: "Menyetujui atau menolak order yang menunggu persetujuan.",
        description_en: "Approve or reject orders awaiting approval.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["orders"]),
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "order-deliveries",
        label: "Pengiriman Order",
        label_en: "Order Deliveries",
        description: "Mencatat jumlah yang diterima penerima dan mengoreksi catatan pengiriman.",
        description_en: "Record quantities received by the recipient and correct delivery records.",
        group: "Sales Tracker",
        module: Some(FeatureModule::SALES_TRACKER),
        requires: Some(&["orders"]),
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: Some(&["STAFF"]),
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "reports",
        label: "Laporan",
        label_en: "Reports",
        description: "Laporan waktu kerja, ekspor PDF/Excel, laporan terjadwal, dan ringkasan manajer.",
        description_en: "Work-hour reports, PDF/Excel exports, scheduled reports, and manager summaries.",
        group: "Reports",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: Some(true),
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "audit-logs",
        label: "Log Audit",
        label_en: "Audit Logs",
        description: "Riwayat perubahan data perusahaan, recycle bin, dan pemulihan baris terhapus.",
        description_en: "History of company data changes, the recycle bin, and restoring deleted rows.",
        group: "Reports",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "billing",
        label: "Langganan & Tagihan",
        label_en: "Subscription & Billing",
        description: "Paket langganan perusahaan, tagihan, dan perubahan/pembatalan langganannya.",
        description_en: "The company's subscription plan, invoices, and plan changes/cancellation.",
        group: "Billing",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "invoices",
        label: "Invoice Timesheet",
        label_en: "Timesheet Invoices",
        description: "Invoice yang dibuat dari timesheet yang sudah disetujui.",
        description_en: "Invoices generated from approved timesheets.",
        group: "Billing",
        module: Some(FeatureModule::TIME_TRACKING),
        requires: Some(&["timesheets"]),
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE],
    },
    FeatureDefinition {
        key: "company-settings",
        label: "Pengaturan Perusahaan",
        label_en: "Company Settings",
        description: "Profil perusahaan, tautan undangan, konfigurasi aplikasi, dan setelan sistem.",
        description_en: "Company profile, invite links, app configuration, and system settings.",
        group: "Settings",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: Some(&["GUEST"]),
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "integrations",
        label: "Integrasi",
        label_en: "Integrations",
        description: "Koneksi perusahaan ke ClickUp, Jira, Discord, dan Google Calendar bersama.",
        description_en: "Company connections to ClickUp, Jira, Discord, and shared Google Calendar.",
        group: "Settings",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "calendar",
        label: "Kalender Pribadi",
        label_en: "Personal Calendar",
        description: "Koneksi Google Calendar milik karyawan sendiri dan acara di dalamnya.",
        description_en: "An employee's own Google Calendar connection and its events.",
        group: "Settings",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web", "mobile"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "table-fields",
        label: "Visibilitas Kolom Tabel",
        label_en: "Table Field Visibility",
        description: "Matriks role × kolom yang menentukan kolom mana disensor di tabel data (docs/table_field_visibility.md).",
        description_en: "The role × column matrix that decides which columns are censored in data tables (docs/table_field_visibility.md).",
        group: "Settings",
        module: None,
        requires: None,
        system_only: None,
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
    FeatureDefinition {
        key: "access-levels",
        label: "Access Level (lama)",
        label_en: "Access Levels (legacy)",
        description: "CRUD tabel `accessLevel` mentah — jalur backoffice lama, digantikan Peran & Hak Akses.",
        description_en: "Raw CRUD on the `accessLevel` table — the old backoffice path, superseded by Roles & Permissions.",
        group: "System",
        module: None,
        requires: None,
        system_only: Some(true),
        surfaces: &["web"],
        person_scoped: None,
        legacy_open: None,
        withheld_from_built_in: None,
        actions: &[READ, WRITE, DELETE],
    },
];

/// `FEATURE_BY_KEY.get(key)`.
pub fn feature_by_key(key: &str) -> Option<&'static FeatureDefinition> {
    FEATURES.iter().find(|f| f.key == key)
}

/// `FEATURE_KEYS` (urutan registry — dipakai pesan `@IsIn(FEATURE_KEYS)`).
pub fn feature_keys() -> impl Iterator<Item = &'static str> {
    FEATURES.iter().map(|f| f.key)
}

/// `requiredFeatureKeys(key)`: penutupan transitif `requires` (BFS, urutan sisip Node).
pub fn required_feature_keys(key: &str) -> Vec<&'static str> {
    let mut result: Vec<&'static str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::from([key]);
    let mut queue: VecDeque<&'static str> = feature_by_key(key)
        .and_then(|f| f.requires)
        .unwrap_or_default()
        .iter()
        .copied()
        .collect();
    while let Some(next) = queue.pop_front() {
        if !seen.insert(next) {
            continue;
        }
        result.push(next);
        queue.extend(
            feature_by_key(next)
                .and_then(|f| f.requires)
                .unwrap_or_default()
                .iter()
                .copied(),
        );
    }
    result
}

/// `TemplatePermission`.
#[derive(Debug, Clone, Serialize)]
pub struct TemplatePermission {
    pub feature: &'static str,
    pub actions: &'static [PermissionAction],
}

/// `RoleTemplate`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleTemplate {
    pub key: &'static str,
    pub name: &'static str,
    pub name_en: &'static str,
    pub description: &'static str,
    pub base_role: BaseRole,
    pub permissions: Vec<TemplatePermission>,
}

const RW: &[PermissionAction] = &[READ, WRITE];
const RWD: &[PermissionAction] = &[READ, WRITE, DELETE];
const R: &[PermissionAction] = &[READ];

/// `grant(actions, ...features)`.
fn grant(
    actions: &'static [PermissionAction],
    features: &[&'static str],
) -> Vec<TemplatePermission> {
    features
        .iter()
        .map(|feature| TemplatePermission { feature, actions })
        .collect()
}

fn concat(parts: Vec<Vec<TemplatePermission>>) -> Vec<TemplatePermission> {
    parts.into_iter().flatten().collect()
}

/// `ROLE_TEMPLATES` (§15.5). `auditor` diturunkan dari registry, seperti Node.
pub static ROLE_TEMPLATES: LazyLock<Vec<RoleTemplate>> = LazyLock::new(|| {
    vec![
        RoleTemplate {
            key: "branch-manager",
            name: "Kepala Cabang",
            name_en: "Branch Manager",
            description: "Memegang seluruh operasional satu cabang: karyawan, absensi, cuti, kunjungan toko, dan laporannya.",
            base_role: BaseRole::ADMIN,
            permissions: concat(vec![
                grant(RWD, &["users", "leaves", "stores", "reports"]),
                grant(RW, &["clock", "store-visits"]),
            ]),
        },
        RoleTemplate {
            key: "sales-supervisor",
            name: "Supervisor Sales",
            name_en: "Sales Supervisor",
            description: "Mengatur toko, wilayah, jadwal dan hasil kunjungan sales, serta membaca laporannya.",
            base_role: BaseRole::MANAGER,
            permissions: concat(vec![
                grant(RW, &["stores", "store-visits", "store-visit-plans"]),
                grant(R, &["visit-masters"]),
                grant(R, &["reports"]),
            ]),
        },
        RoleTemplate {
            key: "field-staff",
            name: "Staff Lapangan",
            name_en: "Field Staff",
            description: "Sales yang bekerja dari aplikasi: absensi, cuti, kunjungan toko, dan data tokonya.",
            base_role: BaseRole::STAFF,
            permissions: grant(RW, &["clock", "leaves", "store-visits", "stores"]),
        },
        RoleTemplate {
            key: "sales-rep",
            name: "Sales Lapangan",
            name_en: "Sales Rep",
            description: "Sales yang menjalankan kunjungan: membaca rencana kunjungan, mencatat hasilnya, dan mengisi form kunjungan.",
            base_role: BaseRole::STAFF,
            permissions: concat(vec![
                grant(R, &["stores"]),
                grant(
                    RW,
                    &["store-visits", "store-visit-forms", "clock", "leaves"],
                ),
                grant(R, &["store-visit-plans"]),
            ]),
        },
        RoleTemplate {
            key: "office-staff",
            name: "Karyawan Kantor",
            name_en: "Office Staff",
            description: "Karyawan yang bekerja di depan komputer: absensi, cuti, dan timesheet miliknya sendiri.",
            base_role: BaseRole::STAFF,
            permissions: grant(RW, &["clock", "leaves", "timesheets"]),
        },
        RoleTemplate {
            key: "hr-admin",
            name: "Admin HR",
            name_en: "HR Admin",
            description: "Mengelola karyawan, kebijakan dan saldo cuti, absensi, serta laporan kehadiran.",
            base_role: BaseRole::ADMIN,
            permissions: concat(vec![
                grant(RWD, &["users", "leaves", "leave-config"]),
                grant(RW, &["clock"]),
                grant(R, &["reports"]),
            ]),
        },
        RoleTemplate {
            key: "finance",
            name: "Finance",
            name_en: "Finance",
            description: "Mengurus invoice timesheet, langganan perusahaan, dan laporan yang jadi dasarnya.",
            base_role: BaseRole::ADMIN,
            permissions: concat(vec![
                grant(RW, &["invoices", "timesheets", "billing"]),
                grant(R, &["reports"]),
            ]),
        },
        RoleTemplate {
            key: "auditor",
            name: "Auditor",
            name_en: "Auditor",
            description: "Bisa membuka seluruh menu perusahaan, tapi tidak bisa mengubah apa pun.",
            base_role: BaseRole::ADMIN,
            permissions: FEATURES
                .iter()
                .filter(|f| !f.is_system_only())
                .map(|f| TemplatePermission {
                    feature: f.key,
                    actions: R,
                })
                .collect(),
        },
    ]
});

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn legacy_open_features_are_the_three_known() {
        let keys: Vec<&str> = FEATURES
            .iter()
            .filter(|f| f.is_legacy_open())
            .map(|f| f.key)
            .collect();
        assert_eq!(
            keys,
            ["screenshots", "activity-logs", "manual-time-approvals"]
        );
    }

    #[test]
    fn requires_closure_is_transitive_and_ordered() {
        assert_eq!(
            required_feature_keys("visit-masters"),
            vec!["store-visit-plans", "stores"]
        );
        assert_eq!(
            required_feature_keys("order-approvals"),
            vec!["orders", "products"]
        );
        assert!(required_feature_keys("users").is_empty());
        assert!(required_feature_keys("nope").is_empty());
    }

    /// `feature.registry.spec.ts`: graf `requires` tanpa siklus, semua target terdaftar.
    #[test]
    fn requires_targets_exist_and_acyclic() {
        for f in FEATURES {
            for dep in f.requires.unwrap_or_default() {
                assert!(feature_by_key(dep).is_some(), "{} -> {dep}", f.key);
            }
            assert!(
                !required_feature_keys(f.key).contains(&f.key),
                "siklus di {}",
                f.key
            );
        }
    }
}
