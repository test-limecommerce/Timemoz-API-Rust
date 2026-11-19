//! Data & algoritme bersama penulis PDF ala `pdfkit` 0.17 (dipakai `mz-billing::pdf` dan
//! `mz-visit-forms::pdf::writer`): metrik AFM Helvetica + kerning ([`afm`]) dan kesempatan putus
//! baris ([`breaks`]). Penulisnya sendiri masih terpisah per crate (perilakunya berbeda: format
//! warna, kompresi aliran, fitur halaman/gambar), lihat `plan/PLAN.md` Fase 4g.

pub mod afm;
pub mod breaks;
