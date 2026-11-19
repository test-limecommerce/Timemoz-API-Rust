//! Penerjemah `csv-parse/sync` 5.5.6 dengan opsi yang dipakai impor libur
//! (`holiday-import.service.ts:parse`): `bom`, `columns` (fungsi huruf kecil + trim),
//! `skip_empty_lines`, `trim`, `max_record_size: 2048`, `info`. Pembatas kolom `,`, kutip
//! `"` (escape = kutip), pembatas baris ditemukan otomatis dari baris pertama
//! (`\r\n`, `\n`, atau `\r`). Mesin keadaan mengikuti `lib/api/index.js` baris per baris
//! supaya pesan galat, nomor baris, dan perilaku trim sama.

/// Satu rekaman data: nomor baris (`info.lines`) dan isi kolom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub line: usize,
    pub fields: Vec<String>,
}

/// Hasil parse: header (sudah huruf kecil + trim) dan rekaman data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub headers: Vec<String>,
    pub records: Vec<Record>,
}

const QUOTE: u8 = b'"';
const DELIM: u8 = b',';
const CR: u8 = 13;
const LF: u8 = 10;
const MAX_RECORD_SIZE: usize = 2048;

/// `__isCharTrimable`: CR, LF, form feed, spasi, tab.
const fn trimable(b: u8) -> bool {
    matches!(b, CR | LF | 0x0C | b' ' | b'\t')
}

/// Spasi putih `String.prototype.trimEnd` (WhiteSpace + LineTerminator ECMAScript).
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{A}' | '\u{B}' | '\u{C}' | '\u{D}' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

struct Machine<'a> {
    buf: &'a [u8],
    lines: usize,
    quoting: bool,
    was_quoting: bool,
    was_row_delimiter: bool,
    escaping: bool,
    field: Vec<u8>,
    record: Vec<String>,
    record_length: usize,
    /// `record_delimiter` yang sudah ditemukan (kosong = belum).
    delimiter: Option<&'static [u8]>,
    headers: Option<Vec<String>>,
    out: Vec<Record>,
}

impl<'a> Machine<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            lines: 1,
            quoting: false,
            was_quoting: false,
            was_row_delimiter: false,
            escaping: false,
            field: Vec::new(),
            record: Vec::new(),
            record_length: 0,
            delimiter: None,
            headers: None,
            out: Vec::new(),
        }
    }

    fn at(&self, pos: usize) -> Option<u8> {
        self.buf.get(pos).copied()
    }

    /// `__autoDiscoverRecordDelimiter`: urutan `\r\n`, `\n`, `\r`; menetapkan pembatas.
    fn discover(&mut self, pos: usize) -> usize {
        let rest = self.buf.get(pos..).unwrap_or_default();
        for rd in [&b"\r\n"[..], b"\n", b"\r"] {
            if rest.starts_with(rd) {
                self.delimiter = Some(rd);
                return rd.len();
            }
        }
        0
    }

    /// `__isRecordDelimiter` (setelah pembatas ditemukan).
    fn record_delimiter_len(&self, pos: usize) -> usize {
        match self.delimiter {
            Some(rd) if self.buf.get(pos..).unwrap_or_default().starts_with(rd) => rd.len(),
            _ => 0,
        }
    }

    /// Nama kolom untuk `__infoField().column` (galat kutip pembuka).
    fn column_label(&self) -> String {
        match &self.headers {
            None => self.record.len().to_string(),
            Some(h) => h
                .get(self.record.len())
                .and_then(|n| serde_json::to_string(n).ok())
                .unwrap_or_else(|| "null".to_owned()),
        }
    }

    /// `__onField`.
    fn on_field(&mut self) {
        let mut text = String::from_utf8_lossy(&self.field).into_owned();
        if !self.was_quoting {
            let trimmed = text.trim_end_matches(is_js_space).len();
            text.truncate(trimmed);
        }
        self.record_length += text.encode_utf16().count();
        self.record.push(text);
        self.field.clear();
        self.was_quoting = false;
    }

    /// `__onRecord` (rekaman pertama = header lewat fungsi `columns`).
    fn on_record(&mut self) -> Result<(), String> {
        let record = std::mem::take(&mut self.record);
        self.record_length = 0;
        let Some(headers) = &self.headers else {
            // `h.trim().toLowerCase()`.
            self.headers = Some(
                record
                    .iter()
                    .map(|h| mz_core::jsfmt::trim(h).to_lowercase())
                    .collect(),
            );
            return Ok(());
        };
        if record.len() != headers.len() {
            return Err(format!(
                "Invalid Record Length: columns length is {}, got {} on line {}",
                headers.len(),
                record.len(),
                self.lines
            ));
        }
        self.out.push(Record {
            line: self.lines,
            fields: record,
        });
        Ok(())
    }

    fn run(mut self) -> Result<Parsed, String> {
        let len = self.buf.len();
        let mut pos = 0;
        while pos < len {
            if self.was_row_delimiter {
                self.lines += 1;
                self.was_row_delimiter = false;
            }
            if !self.quoting && self.delimiter.is_none() {
                self.discover(pos);
            }
            let Some(chr) = self.at(pos) else { break };
            if (chr == CR || chr == LF) && !self.was_row_delimiter {
                self.was_row_delimiter = true;
            }
            if self.escaping {
                self.escaping = false;
            } else {
                // Escape (= kutip) di dalam field berkutip: `""` → satu kutip literal.
                if self.quoting && chr == QUOTE && pos + 1 < len && self.at(pos + 1) == Some(QUOTE)
                {
                    self.escaping = true;
                    pos += 1;
                    continue;
                }
                if chr == QUOTE {
                    if self.quoting {
                        let next = self.at(pos + 1);
                        let next_is_delimiter = next == Some(DELIM);
                        let next_is_record_delimiter = if self.delimiter.is_none() {
                            self.discover(pos + 1) != 0
                        } else {
                            self.record_delimiter_len(pos + 1) != 0
                        };
                        let next_trimable = next.is_some_and(trimable);
                        if next.is_none()
                            || next_is_delimiter
                            || next_is_record_delimiter
                            || next_trimable
                        {
                            self.quoting = false;
                            self.was_quoting = true;
                            pos += 1;
                            continue;
                        }
                        return Err(format!(
                            "Invalid Closing Quote: got \"{}\" at line {} instead of delimiter, record delimiter, trimable character (if activated) or comment",
                            next.map(char::from).unwrap_or_default(),
                            self.lines
                        ));
                    }
                    if self.field.is_empty() {
                        self.quoting = true;
                        pos += 1;
                        continue;
                    }
                    return Err(format!(
                        "Invalid Opening Quote: a quote is found on field {} at line {}, value is {}",
                        self.column_label(),
                        self.lines,
                        serde_json::to_string(&String::from_utf8_lossy(&self.field))
                            .unwrap_or_default()
                    ));
                }
                if !self.quoting {
                    let rd = self.record_delimiter_len(pos);
                    if rd != 0 {
                        if !self.was_quoting && self.record.is_empty() && self.field.is_empty() {
                            // skip_empty_lines
                            pos += rd;
                            continue;
                        }
                        self.on_field();
                        self.on_record()?;
                        pos += rd;
                        continue;
                    }
                    if chr == DELIM {
                        self.on_field();
                        pos += 1;
                        continue;
                    }
                }
            }
            if self.record_length + self.field.len() > MAX_RECORD_SIZE {
                return Err(format!(
                    "Max Record Size: record exceed the maximum number of tolerated bytes of {MAX_RECORD_SIZE} at line {}",
                    self.lines
                ));
            }
            // ltrim + rtrim (`trim: true`).
            let lappend = self.quoting || !self.field.is_empty() || !trimable(chr);
            let rappend = !self.was_quoting;
            if lappend && rappend {
                self.field.push(chr);
            } else if !trimable(chr) {
                return Err(format!(
                    "Invalid Closing Quote: found non trimable byte after quote at line {}",
                    self.lines
                ));
            }
            pos += 1;
        }
        if self.quoting {
            return Err(format!(
                "Quote Not Closed: the parsing is finished with an opening quote at line {}",
                self.lines
            ));
        }
        if self.was_quoting || !self.record.is_empty() || !self.field.is_empty() {
            self.on_field();
            self.on_record()?;
        }
        Ok(Parsed {
            headers: self.headers.unwrap_or_default(),
            records: self.out,
        })
    }
}

/// `parse(buffer, opts)`: `Err(message)` = `CsvError.message`.
pub fn parse(input: &[u8]) -> Result<Parsed, String> {
    // `bom: true`: BOM UTF-8 dibuang; BOM UTF-16LE mengganti penyandian seluruh berkas.
    let decoded;
    let mut buf = input;
    if input.len() >= 3 && input.starts_with(&[0xEF, 0xBB, 0xBF]) {
        buf = input.get(3..).unwrap_or_default();
    } else if input.len() >= 3 && input.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = input
            .get(2..)
            .unwrap_or_default()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        decoded = String::from_utf16_lossy(&units).into_bytes();
        buf = &decoded;
    }
    Machine::new(buf).run()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Parsed {
        parse(s.as_bytes()).unwrap()
    }

    fn err(s: &str) -> String {
        parse(s.as_bytes()).unwrap_err()
    }

    #[test]
    fn dasar_dan_nomor_baris() {
        let p = ok("a,b\n1,2\n\n3,4");
        assert_eq!(p.headers, ["a", "b"]);
        assert_eq!(p.records.iter().map(|r| r.line).collect::<Vec<_>>(), [2, 4]);
        assert_eq!(p.records[1].fields, ["3", "4"]);
    }

    #[test]
    fn header_huruf_kecil_trim_dan_bom() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice(b"Tanggal , NAMA\n1,2\n");
        let p = parse(&b).unwrap();
        assert_eq!(p.headers, ["tanggal", "nama"]);
        assert_eq!(p.records.len(), 1);
    }

    #[test]
    fn crlf_dan_cr() {
        let p = ok("a,b\r\n1,2\r\n3,4\r\n");
        assert_eq!(p.records.len(), 2);
        assert_eq!(p.records[1].line, 3);
        let p = ok("a,b\r1,2\r3,4");
        assert_eq!(p.records.len(), 2);
    }

    #[test]
    fn kutip_dan_escape() {
        let p = ok("a,b\n\"x, y\",\"he said \"\"hi\"\"\"\n");
        assert_eq!(p.records[0].fields, ["x, y", "he said \"hi\""]);
        let p = ok("a\n\"line1\nline2\"\nz");
        assert_eq!(p.records[0].fields, ["line1\nline2"]);
        assert_eq!(p.records[0].line, 3);
        assert_eq!(p.records[1].line, 4);
    }

    #[test]
    fn trim_di_luar_kutip() {
        let p = ok("a,b\n  x  ,\t y \n");
        assert_eq!(p.records[0].fields, ["x", "y"]);
        // Dalam kutip spasi dipertahankan.
        let p = ok("a\n\"  x  \"\n");
        assert_eq!(p.records[0].fields, ["  x  "]);
        // Baris berisi spasi saja = kosong, dilewati.
        let p = ok("a\n   \nz");
        assert_eq!(p.records.len(), 1);
        assert_eq!(p.records[0].line, 3);
    }

    #[test]
    fn galat_dan_pesan() {
        assert_eq!(
            err("a,b\n1,\"x"),
            "Quote Not Closed: the parsing is finished with an opening quote at line 2"
        );
        assert_eq!(
            err("a,b\n1"),
            "Invalid Record Length: columns length is 2, got 1 on line 2"
        );
        assert_eq!(
            err("a,b\n1,2,3\n"),
            "Invalid Record Length: columns length is 2, got 3 on line 2"
        );
        assert_eq!(
            err("a,b\nx\"y,2"),
            "Invalid Opening Quote: a quote is found on field \"a\" at line 2, value is \"x\""
        );
        assert_eq!(
            err("a,b\n\"x\"y,2"),
            "Invalid Closing Quote: got \"y\" at line 2 instead of delimiter, record delimiter, trimable character (if activated) or comment"
        );
        assert_eq!(
            err("a,b\n\"x\" y,2"),
            "Invalid Closing Quote: found non trimable byte after quote at line 2"
        );
    }

    #[test]
    fn batas_ukuran_rekaman() {
        let ok_line = format!("a\n{}\n", "x".repeat(2049));
        assert!(parse(ok_line.as_bytes()).is_ok());
        let bad = format!("a\n{}\n", "x".repeat(2050));
        assert_eq!(
            parse(bad.as_bytes()).unwrap_err(),
            "Max Record Size: record exceed the maximum number of tolerated bytes of 2048 at line 2"
        );
    }

    #[test]
    fn kosong_dan_header_saja() {
        assert_eq!(ok(""), Parsed::default());
        let p = ok("a,b,c");
        assert_eq!(p.headers.len(), 3);
        assert!(p.records.is_empty());
    }

    #[test]
    fn utf16_le_bom() {
        let mut b = vec![0xFF, 0xFE];
        for u in "a,b\n1,2".encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        let p = parse(&b).unwrap();
        assert_eq!(p.records[0].fields, ["1", "2"]);
    }
}
