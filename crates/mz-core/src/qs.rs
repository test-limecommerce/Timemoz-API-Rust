//! Parser query Express 4 (`query parser` = `extended` → `qs` 6.14, `allowPrototypes: true`),
//! diverifikasi terhadap `node_modules` Mudah-API (express 4.22.1, qs 6.14.2) 2026-11-16.
//!
//! Hasil per kunci teratas, urut kemunculan pertama:
//! - `a=1` → [`QVal::One`];
//! - `a=1&a=2`, `a[]=1`, `a[0]=1` → [`QVal::Many`] (indeks diurutkan, lubang dibuang);
//! - `a[b]=1`, indeks ≥ 20, lebih dari 20 nilai, atau campuran array/objek → [`QVal::Obj`].
//!
//! Penyederhanaan: array yang memuat elemen bukan string (`a[0][b]=1`, `a=1&a[b]=2`) juga
//! dilaporkan sebagai [`QVal::Obj`]; di Node keduanya tetap gagal `@IsString()` per elemen.

/// `arrayLimit` default `qs`: indeks `0..20`, paling banyak 20 nilai digabung.
const ARRAY_LIMIT: usize = 20;
/// `parameterLimit` default `qs`.
const PARAMETER_LIMIT: usize = 1000;

/// Nilai satu kunci query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QVal {
    One(String),
    Many(Vec<String>),
    /// Objek (`a[b]=1`) atau bentuk lain yang bukan string / array string.
    Obj,
}

impl QVal {
    /// String tunggal; array/objek → `None`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::One(s) => Some(s),
            Self::Many(_) | Self::Obj => None,
        }
    }

    /// Truthiness JS.
    pub fn truthy(&self) -> bool {
        match self {
            Self::One(s) => !s.is_empty(),
            Self::Many(_) | Self::Obj => true,
        }
    }

    /// `Array.isArray(v) ? v : [v]` dengan elemen di-`String()`-kan.
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v,
            Self::Obj => vec!["[object Object]".to_owned()],
        }
    }

    /// `String(v)`: array → `a,b`, objek → `[object Object]`.
    pub fn js_string(&self) -> String {
        match self {
            Self::One(s) => s.clone(),
            Self::Many(v) => v.join(","),
            Self::Obj => "[object Object]".to_owned(),
        }
    }
}

/// Nilai kunci `key` di hasil [`parse`].
pub fn get<'a>(q: &'a [(String, QVal)], key: &str) -> Option<&'a QVal> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// `utils.decode`: `+` → spasi lalu `decodeURIComponent`; urutan persen tak sah →
/// string apa adanya (sesudah `+` diganti).
pub fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            match hex {
                Some(n) => {
                    out.push(n);
                    i += 3;
                    continue;
                }
                None => return s,
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8(out).unwrap_or(s)
}

/// Elemen array sementara.
#[derive(Debug, Clone)]
enum Elem {
    Str(String),
    Other,
}

/// Nilai sementara (meniru bentuk JS sebelum `compact`).
#[derive(Debug, Clone)]
enum Node {
    Str(String),
    /// Array jarang (`None` = lubang).
    Arr(Vec<Option<Elem>>),
    Obj,
}

/// Segmen pertama sesudah kunci dasar.
enum Child {
    Push,
    Index(usize),
    Key,
}

/// `(kunci dasar, segmen pertama, ada segmen lanjutan?)`. `None` segmen = kunci polos.
fn split_key(key: &str) -> (String, Option<(Child, bool)>) {
    // Segmen sah: `[` … `]` tanpa `[`/`]` di dalamnya.
    let segment_at = |from: usize| -> Option<(usize, usize)> {
        let rest = key.get(from..)?;
        let mut search = 0;
        while let Some(rel) = rest.get(search..).and_then(|r| r.find('[')) {
            let open = search + rel;
            let after = rest.get(open + 1..)?;
            match after.find(['[', ']']) {
                Some(j) if after.as_bytes().get(j) == Some(&b']') => {
                    return Some((from + open, from + open + 1 + j));
                }
                _ => search = open + 1,
            }
        }
        None
    };
    let Some((open, close)) = segment_at(0) else {
        return (key.to_owned(), None);
    };
    let inner = |o: usize, c: usize| key.get(o + 1..c).unwrap_or_default().to_owned();
    let (base, child_seg) = if open == 0 {
        // `[a]=1` → kunci `a`; segmen berikutnya (bila ada) jadi anak.
        let base = inner(open, close);
        match segment_at(close + 1) {
            Some(next) => (base, Some(next)),
            None => return (base, None),
        }
    } else {
        (
            key.get(..open).unwrap_or_default().to_owned(),
            Some((open, close)),
        )
    };
    let Some((o, c)) = child_seg else {
        return (base, None);
    };
    let text = inner(o, c);
    let nested = segment_at(c + 1).is_some();
    let child = if text.is_empty() {
        Child::Push
    } else {
        match text.parse::<usize>() {
            Ok(i) if i.to_string() == text && i < ARRAY_LIMIT => Child::Index(i),
            _ => Child::Key,
        }
    };
    (base, Some((child, nested)))
}

/// `utils.merge(target, source)` untuk bentuk yang dimodelkan.
fn merge(target: Node, source: Node) -> Node {
    match (target, source) {
        (Node::Obj, _) | (_, Node::Obj) => Node::Obj,
        (Node::Str(t), Node::Str(s)) => Node::Arr(vec![Some(Elem::Str(t)), Some(Elem::Str(s))]),
        (Node::Str(t), Node::Arr(src)) => {
            let mut out = vec![Some(Elem::Str(t))];
            out.extend(src);
            Node::Arr(out)
        }
        (Node::Arr(mut t), Node::Str(s)) => {
            t.push(Some(Elem::Str(s)));
            Node::Arr(t)
        }
        (Node::Arr(mut t), Node::Arr(src)) => {
            for (i, item) in src.into_iter().enumerate() {
                let Some(item) = item else { continue };
                match t.get_mut(i) {
                    Some(slot @ None) => *slot = Some(item),
                    Some(Some(_)) => t.push(Some(item)),
                    None => {
                        t.resize(i, None);
                        t.push(Some(item));
                    }
                }
            }
            Node::Arr(t)
        }
    }
}

/// Parse query mentah (tanpa `?`).
pub fn parse(raw: &str) -> Vec<(String, QVal)> {
    // Tahap 1 (`parseValues`): kunci mentah terdekode identik digabung (`combine`).
    let mut temp: Vec<(String, Node)> = Vec::new();
    for part in raw.split('&').take(PARAMETER_LIMIT) {
        if part.is_empty() {
            continue;
        }
        let pos = part.find("]=").map(|p| p + 1).or_else(|| part.find('='));
        let (k, v) = match pos {
            Some(p) => (
                part.get(..p).unwrap_or_default(),
                part.get(p + 1..).unwrap_or_default(),
            ),
            None => (part, ""),
        };
        let (key, val) = (decode(k), decode(v));
        match temp.iter_mut().find(|(t, _)| *t == key) {
            Some((_, slot)) => {
                let prev = std::mem::replace(slot, Node::Obj);
                *slot = match merge(prev, Node::Str(val)) {
                    Node::Arr(a) if a.len() > ARRAY_LIMIT => Node::Obj,
                    other => other,
                };
            }
            None => temp.push((key, Node::Str(val))),
        }
    }

    // Tahap 2 (`parseKeys` + `merge`).
    let mut out: Vec<(String, Node)> = Vec::new();
    for (raw_key, val) in temp {
        if raw_key.is_empty() {
            continue;
        }
        let (base, child) = split_key(&raw_key);
        if base.is_empty() || base == "__proto__" {
            continue;
        }
        let leaf = match child {
            None => val,
            Some((Child::Key, _)) => Node::Obj,
            Some((_, true)) if matches!(val, Node::Obj) => Node::Obj,
            Some((Child::Push, nested)) => match val {
                _ if nested => Node::Arr(vec![Some(Elem::Other)]),
                Node::Str(s) => Node::Arr(vec![Some(Elem::Str(s))]),
                arr @ Node::Arr(_) => arr,
                Node::Obj => Node::Obj,
            },
            Some((Child::Index(i), nested)) => {
                let elem = match val {
                    Node::Str(s) if !nested => Elem::Str(s),
                    _ => Elem::Other,
                };
                let mut a = vec![None; i];
                a.push(Some(elem));
                Node::Arr(a)
            }
        };
        match out.iter_mut().find(|(k, _)| *k == base) {
            Some((_, slot)) => {
                let prev = std::mem::replace(slot, Node::Obj);
                *slot = merge(prev, leaf);
            }
            None => out.push((base, leaf)),
        }
    }

    out.into_iter()
        .map(|(k, node)| {
            let v = match node {
                Node::Str(s) => QVal::One(s),
                Node::Obj => QVal::Obj,
                Node::Arr(items) => {
                    let items: Option<Vec<String>> = items
                        .into_iter()
                        .flatten()
                        .map(|e| match e {
                            Elem::Str(s) => Some(s),
                            Elem::Other => None,
                        })
                        .collect();
                    items.map_or(QVal::Obj, QVal::Many)
                }
            };
            (k, v)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(s: &str) -> QVal {
        QVal::One(s.into())
    }
    fn many(v: &[&str]) -> QVal {
        QVal::Many(v.iter().map(|s| (*s).to_owned()).collect())
    }
    fn rep(pat: &str, n: usize) -> String {
        (0..n)
            .map(|i| pat.replace('#', &i.to_string()))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Dari `app.get('query parser fn')` Express 4.22.1 / qs 6.14.2.
    #[test]
    fn matches_express() {
        let a = |v: QVal| vec![("a".to_owned(), v)];
        let cases: Vec<(String, Vec<(String, QVal)>)> = vec![
            ("a=1".into(), a(one("1"))),
            ("a=1&a=2".into(), a(many(&["1", "2"]))),
            ("a[]=1".into(), a(many(&["1"]))),
            ("a[0]=1".into(), a(many(&["1"]))),
            ("a[1]=x&a[0]=y".into(), a(many(&["y", "x"]))),
            ("a[19]=1".into(), a(many(&["1"]))),
            ("a[20]=1".into(), a(QVal::Obj)),
            ("a[21]=1".into(), a(QVal::Obj)),
            ("a[b]=1".into(), a(QVal::Obj)),
            ("a=1&a[]=2".into(), a(many(&["1", "2"]))),
            ("a[]=1&a=2".into(), a(many(&["1", "2"]))),
            ("a=1&a[b]=2".into(), a(QVal::Obj)),
            ("a[b]=1&a=2".into(), a(QVal::Obj)),
            ("a".into(), a(one(""))),
            ("a=".into(), a(one(""))),
            ("=1".into(), vec![]),
            ("a=%ZZ".into(), a(one("%ZZ"))),
            ("a=x%20y+z".into(), a(one("x y z"))),
            ("a%5B%5D=1".into(), a(many(&["1"]))),
            ("a[0][b]=1".into(), a(QVal::Obj)),
            ("a[]=1&a[]=2&a[]=3".into(), a(many(&["1", "2", "3"]))),
            ("[a]=1".into(), a(one("1"))),
            ("a]=1".into(), vec![("a]".into(), one("1"))]),
            ("a[=1".into(), vec![("a[".into(), one("1"))]),
            ("a[]b=1".into(), a(many(&["1"]))),
            ("a[x]y=1".into(), a(QVal::Obj)),
            ("a.b=1".into(), vec![("a.b".into(), one("1"))]),
            (
                "a=1&&b=2".into(),
                vec![("a".into(), one("1")), ("b".into(), one("2"))],
            ),
            ("a==1".into(), a(one("=1"))),
            ("a[]=%E0%A4%A".into(), a(many(&["%E0%A4%A"]))),
            ("__proto__=1".into(), vec![]),
            ("toString=1".into(), vec![("toString".into(), one("1"))]),
            ("a[ ]=1".into(), a(QVal::Obj)),
            ("a[-1]=1".into(), a(QVal::Obj)),
            ("a[01]=1".into(), a(QVal::Obj)),
            ("a[0]=x&a[0]=y".into(), a(QVal::Obj)),
            (rep("a=#", 21), a(QVal::Obj)),
            (rep("a[]=#", 21), a(QVal::Obj)),
            (
                rep("a[]=#", 20),
                a(QVal::Many((0..20).map(|i| i.to_string()).collect())),
            ),
            ("a[0]=x&a[2]=z".into(), a(many(&["x", "z"]))),
            ("a[2]=z&a=1".into(), a(many(&["z", "1"]))),
            ("a[b]=1&a[c]=2".into(), a(QVal::Obj)),
            ("a[0]=1&a[b]=2".into(), a(QVal::Obj)),
            ("a[]=1&a[b]=2".into(), a(QVal::Obj)),
            ("a[b]=1&a[]=2".into(), a(QVal::Obj)),
            ("a[0][b]=1&a[]=2".into(), a(QVal::Obj)),
            ("a=1;b=2".into(), a(one("1;b=2"))),
            ("a=%".into(), a(one("%"))),
            ("a=%2".into(), a(one("%2"))),
            ("%5B=1".into(), vec![("[".into(), one("1"))]),
            ("a%5Bb%5D=1".into(), a(QVal::Obj)),
            ("a[%5D=1".into(), a(many(&["1"]))),
        ];
        for (raw, want) in cases {
            assert_eq!(parse(&raw), want, "{raw}");
        }
        assert_eq!(parse(&rep("k#=1", 1100)).len(), 1000);
    }

    #[test]
    fn helpers() {
        let q = parse("a=1&b[]=x&b[]=y&c[d]=1&e=");
        assert_eq!(get(&q, "a").and_then(QVal::as_str), Some("1"));
        assert_eq!(get(&q, "b").map(QVal::js_string).as_deref(), Some("x,y"));
        assert_eq!(
            get(&q, "c").map(QVal::js_string).as_deref(),
            Some("[object Object]")
        );
        assert_eq!(get(&q, "e").map(QVal::truthy), Some(false));
        assert!(get(&q, "z").is_none());
    }
}
