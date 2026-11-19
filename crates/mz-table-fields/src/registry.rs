//! Daftar induk "kolom apa saja yang bisa diatur visibilitasnya" (`table-field.registry.ts`).
//!
//! Sumber kebenaran ada DI SINI, bukan di baris database: DB hanya menyimpan penyimpangan dari
//! default `visible`. `key` tabel/kolom TERSIMPAN di DB; menambah aman, mengganti nama = migrasi.

/// `FieldVisibilityMode` (enum Postgres `"FieldVisibilityMode"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldVisibilityMode {
    Visible,
    Revealable,
    Masked,
    Hidden,
}

/// `FIELD_VISIBILITY_MODES` (urutan dipakai pesan `@IsIn`).
pub const FIELD_VISIBILITY_MODES: [&str; 4] = ["visible", "revealable", "masked", "hidden"];

impl FieldVisibilityMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Revealable => "revealable",
            Self::Masked => "masked",
            Self::Hidden => "hidden",
        }
    }

    /// `isFieldVisibilityMode`: nilai tak dikenal = `None` (diabaikan, bukan dianggap menyensor).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "visible" => Self::Visible,
            "revealable" => Self::Revealable,
            "masked" => Self::Masked,
            "hidden" => Self::Hidden,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ColumnDefinition {
    /// Stabil selamanya — tersimpan di DB.
    pub key: &'static str,
    /// Jalur di dalam objek respons; `None` = sama dengan `key`.
    pub path: Option<&'static str>,
    pub label: &'static str,
    pub label_en: &'static str,
    /// Tak pernah bisa disensor (§0.12).
    pub locked: bool,
    /// Kosong = semua mode boleh.
    pub allowed_modes: &'static [FieldVisibilityMode],
    pub require_reason: bool,
    pub searchable: bool,
    pub sortable: bool,
}

impl ColumnDefinition {
    const fn new(key: &'static str, label: &'static str, label_en: &'static str) -> Self {
        Self {
            key,
            path: None,
            label,
            label_en,
            locked: false,
            allowed_modes: &[],
            require_reason: false,
            searchable: false,
            sortable: false,
        }
    }
    const fn locked(mut self) -> Self {
        self.locked = true;
        self
    }
    const fn searchable(mut self) -> Self {
        self.searchable = true;
        self
    }
    const fn sortable(mut self) -> Self {
        self.sortable = true;
        self
    }
    /// Jalur nilai di record respons (`column.path ?? column.key`).
    pub fn response_path(&self) -> &'static str {
        self.path.unwrap_or(self.key)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TableDefinition {
    pub key: &'static str,
    pub label: &'static str,
    pub label_en: &'static str,
    /// Fitur `feature.registry.ts` pemilik tabel.
    pub feature_key: &'static str,
    /// Fitur LAIN yang route-nya membawa baris tabel ini sebagai anak sebuah record.
    pub embedded_in: &'static [&'static str],
    /// Token resolver reveal (§7.3). `None` = tabel tanpa kolom yang layak di-reveal.
    pub reveal_resolver: Option<&'static str>,
    pub columns: &'static [ColumnDefinition],
}

/// Token resolver reveal tabel `stores` (diisi modul store-visits).
pub const STORES_REVEAL_RESOLVER: &str = "STORES_REVEAL_RESOLVER";
/// Token resolver reveal tabel `customers` (diisi modul customers).
pub const CUSTOMERS_REVEAL_RESOLVER: &str = "CUSTOMERS_REVEAL_RESOLVER";

const fn c(key: &'static str, label: &'static str, label_en: &'static str) -> ColumnDefinition {
    ColumnDefinition::new(key, label, label_en)
}

const STORES_COLUMNS: &[ColumnDefinition] = &[
    c("code", "Kode Toko", "Store Code")
        .locked()
        .searchable()
        .sortable(),
    c("name", "Nama Toko", "Store Name")
        .locked()
        .searchable()
        .sortable(),
    c("ownerName", "Nama Pemilik", "Owner Name").searchable(),
    c("phone", "No. Telepon", "Phone"),
    c("address", "Alamat", "Address").searchable(),
    c("lat", "Latitude", "Latitude").locked(),
    c("lng", "Longitude", "Longitude").locked(),
    c("radiusMeters", "Radius", "Radius"),
    c("group", "Wilayah", "Region"),
    c("totalVisits", "Total Kunjungan", "Total Visits"),
    c("lastVisitAt", "Kunjungan Terakhir", "Last Visit").sortable(),
    c(
        "lastVisitStatus",
        "Status Kunjungan Terakhir",
        "Last Visit Status",
    ),
    c("isActive", "Status", "Status"),
    c("createdAt", "Dibuat", "Created At").sortable(),
    c("updatedAt", "Diubah", "Updated At"),
];

const CUSTOMERS_COLUMNS: &[ColumnDefinition] = &[
    c("code", "Kode Customer", "Customer Code")
        .locked()
        .searchable()
        .sortable(),
    c("name", "Nama Customer", "Customer Name")
        .locked()
        .searchable()
        .sortable(),
    c("type", "Tipe", "Type"),
    c("group", "Group", "Group"),
    c("channel", "Channel", "Channel"),
    c("subChannel", "Sub Channel", "Sub Channel"),
    c("taxNumber", "NPWP", "Tax Number").searchable(),
    c("phone", "No. Telepon", "Phone"),
    c("isActive", "Status", "Status"),
    c("storeCount", "Jumlah Toko", "Store Count"),
    c("createdAt", "Dibuat", "Created At").sortable(),
    c("updatedAt", "Diubah", "Updated At"),
];

const PRODUCTS_COLUMNS: &[ColumnDefinition] = &[
    c("sku", "SKU", "SKU").locked().searchable().sortable(),
    c("name", "Nama Produk", "Product Name")
        .locked()
        .searchable()
        .sortable(),
    c("category", "Kategori", "Category"),
    c("subCategory", "Sub Kategori", "Sub Category"),
    c("brand", "Brand", "Brand"),
    c("principal", "Principal", "Principal"),
    c("baseUom", "Satuan Dasar", "Base UOM"),
    c("taxRate", "Pajak", "Tax"),
    c("weight", "Berat", "Weight"),
    c("volume", "Volume", "Volume"),
    c("barcode", "Barcode", "Barcode").searchable(),
    c("isOrderable", "Bisa Dipesan", "Orderable"),
    c("isActive", "Status", "Status"),
    c("createdAt", "Dibuat", "Created At").sortable(),
    c("updatedAt", "Diubah", "Updated At").sortable(),
];

const ORDERS_COLUMNS: &[ColumnDefinition] = &[
    c("orderNumber", "No. Order", "Order Number")
        .locked()
        .searchable()
        .sortable(),
    c("status", "Status", "Status").locked(),
    c("subtotal", "Subtotal", "Subtotal"),
    c("discountTotal", "Total Diskon", "Discount Total"),
    c("taxTotal", "Total Pajak", "Tax Total"),
    c("grandTotal", "Total Akhir", "Grand Total").sortable(),
    c("finalSubtotal", "Subtotal Terkirim", "Delivered Subtotal"),
    c(
        "finalDiscountTotal",
        "Total Diskon Terkirim",
        "Delivered Discount Total",
    ),
    c(
        "finalTaxTotal",
        "Total Pajak Terkirim",
        "Delivered Tax Total",
    ),
    c(
        "finalGrandTotal",
        "Total Akhir Terkirim",
        "Delivered Grand Total",
    ),
];

const ORDER_ITEMS_COLUMNS: &[ColumnDefinition] = &[
    c("listUnitPrice", "Harga List", "List Unit Price"),
    c("unitPrice", "Harga Satuan", "Unit Price"),
    c("discountPercent", "Diskon (%)", "Discount Percent"),
    c("discountAmount", "Diskon (Nominal)", "Discount Amount"),
    c("netAmount", "Nilai Bersih", "Net Amount"),
    c("lineTotal", "Total Baris", "Line Total"),
    c(
        "finalGrossAmount",
        "Nilai Kotor Terkirim",
        "Delivered Gross Amount",
    ),
    c(
        "finalDiscountValue",
        "Diskon Terkirim",
        "Delivered Discount",
    ),
    c(
        "finalNetAmount",
        "Nilai Bersih Terkirim",
        "Delivered Net Amount",
    ),
    c(
        "finalLineTotal",
        "Total Baris Terkirim",
        "Delivered Line Total",
    ),
];

/// `TABLES` (urutan registry).
pub const TABLES: &[TableDefinition] = &[
    TableDefinition {
        key: "stores",
        label: "Toko",
        label_en: "Stores",
        feature_key: "stores",
        embedded_in: &["store-visits", "store-visit-plans"],
        reveal_resolver: Some(STORES_REVEAL_RESOLVER),
        columns: STORES_COLUMNS,
    },
    TableDefinition {
        key: "customers",
        label: "Customer",
        label_en: "Customers",
        feature_key: "customers",
        embedded_in: &["stores"],
        reveal_resolver: Some(CUSTOMERS_REVEAL_RESOLVER),
        columns: CUSTOMERS_COLUMNS,
    },
    TableDefinition {
        key: "products",
        label: "Produk",
        label_en: "Products",
        feature_key: "products",
        embedded_in: &[],
        reveal_resolver: None,
        columns: PRODUCTS_COLUMNS,
    },
    TableDefinition {
        key: "orders",
        label: "Order",
        label_en: "Orders",
        feature_key: "orders",
        embedded_in: &["order-deliveries"],
        reveal_resolver: None,
        columns: ORDERS_COLUMNS,
    },
    TableDefinition {
        key: "order_items",
        label: "Item Order",
        label_en: "Order Items",
        feature_key: "orders",
        embedded_in: &["orders", "order-deliveries"],
        reveal_resolver: None,
        columns: ORDER_ITEMS_COLUMNS,
    },
];

/// `TABLE_KEYS` (urutan pesan `@IsIn`).
pub fn table_keys() -> Vec<&'static str> {
    TABLES.iter().map(|t| t.key).collect()
}

/// `TABLE_BY_KEY.get`.
pub fn table_by_key(key: &str) -> Option<&'static TableDefinition> {
    TABLES.iter().find(|t| t.key == key)
}

pub fn is_table_key(key: &str) -> bool {
    table_by_key(key).is_some()
}

/// `columnOf`.
pub fn column_of(table_key: &str, column_key: &str) -> Option<&'static ColumnDefinition> {
    table_by_key(table_key)?
        .columns
        .iter()
        .find(|c| c.key == column_key)
}

/// `configurableColumns`: semua kolom kecuali `locked`.
pub fn configurable_columns(table_key: &str) -> Vec<&'static ColumnDefinition> {
    table_by_key(table_key)
        .map(|t| t.columns.iter().filter(|c| !c.locked).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_shape() {
        assert_eq!(
            table_keys(),
            ["stores", "customers", "products", "orders", "order_items"]
        );
        assert_eq!(configurable_columns("stores").len(), 11);
        assert!(column_of("stores", "lat").is_some_and(|c| c.locked));
        assert!(column_of("stores", "nope").is_none());
        assert!(table_by_key("products").is_some_and(|t| t.reveal_resolver.is_none()));
    }

    #[test]
    fn mode_roundtrip() {
        for m in FIELD_VISIBILITY_MODES {
            assert_eq!(
                FieldVisibilityMode::parse(m).map(FieldVisibilityMode::as_str),
                Some(m)
            );
        }
        assert!(FieldVisibilityMode::parse("hide").is_none());
    }
}
