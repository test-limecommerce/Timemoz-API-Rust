//! Port `src/quickstart-reminder/quickstart-reminder.content.ts`: urutan pengingat dan
//! teks email per jenis. Teks adalah kontrak (disalin persis).

/// `QuickstartReminderType` (enum Postgres `"QuickstartReminderType"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderType {
    CompanySetting,
    ManageRegion,
    ManageCustomer,
    ManageStore,
    VisitPlan,
    AppInstall,
}

impl ReminderType {
    /// Nilai enum Prisma/Postgres.
    pub const fn as_str(self) -> &'static str {
        match self {
            ReminderType::CompanySetting => "COMPANY_SETTING",
            ReminderType::ManageRegion => "MANAGE_REGION",
            ReminderType::ManageCustomer => "MANAGE_CUSTOMER",
            ReminderType::ManageStore => "MANAGE_STORE",
            ReminderType::VisitPlan => "VISIT_PLAN",
            ReminderType::AppInstall => "APP_INSTALL",
        }
    }
}

/// Kolom stempel `companyQuickstart` yang dipakai rantai pengingat (`QuickstartColumn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampColumn {
    CompanySetting,
    ManageRegion,
    ManageCustomer,
    ManageStore,
    VisitPlan,
}

/// `REMINDER_CHAIN`: pengingat langkah ke-N baru bisa jatuh setelah langkah N-1 dicentang,
/// dihitung 24 jam dari stempel langkah N-1 (langkah pertama: dari lahirnya baris
/// quickstart). `INVITE_TEAM` sengaja tidak ikut.
pub const REMINDER_CHAIN: [(ReminderType, StampColumn); 5] = [
    (ReminderType::CompanySetting, StampColumn::CompanySetting),
    (ReminderType::ManageRegion, StampColumn::ManageRegion),
    (ReminderType::ManageCustomer, StampColumn::ManageCustomer),
    (ReminderType::ManageStore, StampColumn::ManageStore),
    (ReminderType::VisitPlan, StampColumn::VisitPlan),
];

/// `ReminderContent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderContent {
    pub subject: &'static str,
    /// Field `text` Mailtrap (fallback teks polos).
    pub text: &'static str,
    pub category: &'static str,
    pub preheader: &'static str,
    pub paragraphs: Vec<String>,
    /// Daftar bernomor opsional (dipakai APP_INSTALL).
    pub steps: Option<&'static [&'static str]>,
    /// Paragraf setelah daftar langkah.
    pub closing: Option<&'static str>,
    /// Kalimat tebal "Ingat: ..." — tidak ada di APP_INSTALL.
    pub notice: Option<&'static str>,
    pub cta_label: &'static str,
    /// Path dashboard, sama dengan `href` QUICKSTART_STEPS di Mudah-Dashboard.
    pub cta_path: &'static str,
    /// CTA menunjuk ke link download aplikasi (kalau env-nya diisi); `cta_path` cadangan.
    pub cta_prefers_download_link: bool,
}

const NOTICE_SETTING: &str = "Ingat: pastikan seluruh anggota tim juga sudah install aplikasi Timemoz di HP masing-masing. Tanpa ini, tracking tidak akan tercatat meski setting sudah selesai.";
const NOTICE_DATA: &str = "Ingat: pastikan seluruh anggota tim juga sudah install aplikasi Timemoz di HP masing-masing. Tanpa ini, tracking tidak akan tercatat meski data customer sudah lengkap.";

const APP_INSTALL_STEPS: &[&str] = &[
    "Bagikan link download ke semua anggota tim",
    "Setiap orang install & login pakai akun masing-masing",
    "Tracking otomatis aktif setelah login pertama",
];

fn paragraphs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Node: `quickstart-reminder.content.ts:72` `reminderContent(type, { progressPercent })`.
pub fn reminder_content(kind: ReminderType, progress_percent: u32) -> ReminderContent {
    match kind {
        ReminderType::CompanySetting => ReminderContent {
            subject: "Company Setting Reminder",
            text: "Company Setting Reminder",
            category: "Quickstart Company Setting Reminder",
            preheader: "Setting geofence kantor sekali, dan absensi berjalan otomatis.",
            paragraphs: paragraphs(&[
                "Company Setting Anda belum diatur — artinya sistem belum bisa memvalidasi lokasi absensi tim secara otomatis.",
                "Tanpa ini, Anda kehilangan kontrol atas kehadiran tim di lapangan. Setting geofence kantor sekali, dan absensi berjalan otomatis, akurat, tanpa perlu pengawasan manual.",
            ]),
            steps: None,
            closing: None,
            notice: Some(NOTICE_SETTING),
            cta_label: "Aktifkan Company Setting",
            cta_path: "/company?tab=geofence",
            cta_prefers_download_link: false,
        },
        ReminderType::ManageRegion => ReminderContent {
            subject: "Manage Region Reminder",
            text: "Manage Region Reminder",
            category: "Quickstart Manage Region Reminder",
            preheader: "Bagi wilayah kerja sekarang, setiap sales hanya melihat customer di areanya.",
            paragraphs: paragraphs(&[
                "Manage Region belum diisi — tim sales Anda saat ini belum terikat pada wilayah kerja tertentu.",
                "Ini berisiko: data customer tercampur antar sales, laporan jadi sulit dipetakan per area. Bagi wilayah kerja sekarang, dan setiap sales otomatis hanya melihat customer di areanya.",
            ]),
            steps: None,
            closing: None,
            notice: Some(NOTICE_SETTING),
            cta_label: "Atur Pembagian Region",
            cta_path: "/store-groups",
            cta_prefers_download_link: false,
        },
        ReminderType::ManageCustomer => ReminderContent {
            subject: "Manage Customer Reminder",
            text: "Manage Customer Reminder",
            category: "Quickstart Manage Customer Reminder",
            preheader: "Isi data customer sekarang untuk visibilitas penuh kunjungan tim.",
            paragraphs: paragraphs(&[
                "Data customer di sistem Anda masih kosong — ini artinya tim sales belum bisa mulai kunjungan atau dilacak kinerjanya.",
                "Isi data customer sekarang, dan Anda langsung dapat visibilitas penuh: siapa dikunjungi, kapan, dan hasilnya apa.",
            ]),
            steps: None,
            closing: None,
            notice: Some(NOTICE_DATA),
            cta_label: "Lengkapi Data Customer",
            cta_path: "/customers",
            cta_prefers_download_link: false,
        },
        ReminderType::ManageStore => ReminderContent {
            subject: "Manage Store Reminder",
            text: "Manage Store Reminder",
            category: "Quickstart Manage Store Reminder",
            preheader: "Lengkapi data store agar rute kunjungan bisa disusun efisien.",
            paragraphs: paragraphs(&[
                "Daftar store customer Anda masih kosong — tim lapangan belum punya acuan toko mana yang harus dikunjungi.",
                "Lengkapi data store sekarang, agar rute kunjungan bisa disusun efisien dan progres tiap toko terpantau langsung dari dashboard.",
            ]),
            steps: None,
            closing: None,
            notice: Some(NOTICE_DATA),
            cta_label: "Lengkapi Data Store",
            cta_path: "/stores",
            cta_prefers_download_link: false,
        },
        ReminderType::VisitPlan => ReminderContent {
            subject: "Visit Plan Reminder",
            text: "Visit Plan Reminder",
            category: "Quickstart Visit Plan Reminder",
            preheader: "Tinggal satu langkah lagi: atur Visit Plan.",
            paragraphs: vec![
                format!(
                    "Setup Anda sudah {progress_percent}% — tinggal satu langkah lagi: atur Visit Plan."
                ),
                "Setelah ini selesai, Anda punya kontrol penuh atas jadwal kunjungan tim dan bisa memantau hasilnya secara real-time, tanpa laporan manual.".to_owned(),
            ],
            steps: None,
            closing: None,
            notice: Some(NOTICE_DATA),
            cta_label: "Atur Visit Plan",
            cta_path: "/store-visit-plans",
            cta_prefers_download_link: false,
        },
        ReminderType::AppInstall => ReminderContent {
            subject: "Tim Anda belum bisa di-tracking — 1 langkah yang sering terlewat",
            text: "App Install Reminder",
            category: "Quickstart App Install Reminder",
            preheader: "Setup dashboard selesai, tapi tracking baru jalan setelah tim install aplikasi di HP.",
            paragraphs: paragraphs(&[
                "Banyak business owner melewatkan langkah ini: dashboard sudah diatur, tapi tracking kunjungan tim belum bisa jalan karena aplikasi mobile belum diinstall di HP masing-masing anggota tim.",
                "Absensi, geofence, dan kunjungan toko — semuanya tercatat lewat aplikasi di HP tim, bukan cuma dari dashboard.",
                "Yang perlu dilakukan:",
            ]),
            steps: Some(APP_INSTALL_STEPS),
            closing: Some(
                "Tanpa langkah ini, sistem tetap kosong meski setup dashboard sudah lengkap.",
            ),
            notice: None,
            cta_label: "Kirim Link Download ke Tim",
            cta_path: "/employee",
            cta_prefers_download_link: true,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const ALL: [ReminderType; 6] = [
        ReminderType::CompanySetting,
        ReminderType::ManageRegion,
        ReminderType::ManageCustomer,
        ReminderType::ManageStore,
        ReminderType::VisitPlan,
        ReminderType::AppInstall,
    ];

    #[test]
    fn enum_values_match_prisma() {
        let names: Vec<&str> = ALL.iter().map(|t| t.as_str()).collect();
        assert_eq!(
            names,
            [
                "COMPANY_SETTING",
                "MANAGE_REGION",
                "MANAGE_CUSTOMER",
                "MANAGE_STORE",
                "VISIT_PLAN",
                "APP_INSTALL"
            ]
        );
    }

    #[test]
    fn visit_plan_mentions_progress() {
        let c = reminder_content(ReminderType::VisitPlan, 71);
        assert_eq!(c.subject, "Visit Plan Reminder");
        assert_eq!(
            c.paragraphs.first().map(String::as_str),
            Some("Setup Anda sudah 71% — tinggal satu langkah lagi: atur Visit Plan.")
        );
    }

    #[test]
    fn only_app_install_has_steps_and_prefers_download() {
        for t in ALL {
            let c = reminder_content(t, 0);
            let app = t == ReminderType::AppInstall;
            assert_eq!(c.steps.is_some(), app, "{t:?}");
            assert_eq!(c.closing.is_some(), app, "{t:?}");
            assert_eq!(c.notice.is_none(), app, "{t:?}");
            assert_eq!(c.cta_prefers_download_link, app, "{t:?}");
        }
        let c = reminder_content(ReminderType::AppInstall, 100);
        assert_eq!(
            c.subject,
            "Tim Anda belum bisa di-tracking — 1 langkah yang sering terlewat"
        );
        assert_eq!(c.text, "App Install Reminder");
        assert_eq!(c.cta_path, "/employee");
    }

    #[test]
    fn chain_order_excludes_invite_team_and_app_install() {
        let types: Vec<&str> = REMINDER_CHAIN.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            types,
            [
                "COMPANY_SETTING",
                "MANAGE_REGION",
                "MANAGE_CUSTOMER",
                "MANAGE_STORE",
                "VISIT_PLAN"
            ]
        );
    }
}
