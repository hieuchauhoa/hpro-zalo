//! Offline, read-only message search over HPro Zalo's local WebKit IndexedDB data.
//!
//! Zalo's own search needs its native SQLite module, which HPro Zalo does not ship, so this module
//! reads the `message` object store directly (on a temporary copy, never touching the live files)
//! and shows the results in a small GTK window. Launched via `hpro-zalo --search` (tray menu entry).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;

use anyhow::{Context, Result};
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use unicode_normalization::UnicodeNormalization;

const INDEX_USER_SENDDTTM_MSGID: i64 = 5;
const MAX_ROWS: usize = 3000;

#[derive(Debug, Clone)]
pub struct Hit {
    pub ts_ms: f64,
    pub conv_id: String,
    pub sender: String,
    pub text: String,
    pub attach: String,
}

// ---------------------------------------------------------------------------------------------
// Text folding (case + Vietnamese diacritics insensitive)
// ---------------------------------------------------------------------------------------------

fn fold_char(ch: char) -> String {
    if ch == 'đ' || ch == 'Đ' {
        return "d".to_string();
    }
    ch.to_string()
        .nfd()
        .filter(|c| !('\u{0300}'..='\u{036f}').contains(c))
        .flat_map(char::to_lowercase)
        .collect()
}

thread_local! {
    static FOLD_CACHE: RefCell<HashMap<char, String>> = RefCell::new(HashMap::new());
}

pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii() {
            out.push(ch.to_ascii_lowercase());
        } else {
            FOLD_CACHE.with(|c| {
                let mut c = c.borrow_mut();
                out.push_str(c.entry(ch).or_insert_with(|| fold_char(ch)));
            });
        }
    }
    out
}

/// Char-index ranges of `text` matching `needle` (already folded unless `exact`).
fn match_ranges(text: &str, needle: &str, exact: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let (hay, idx): (Vec<char>, Vec<usize>) = if exact {
        (chars.clone(), (0..chars.len()).collect())
    } else {
        let mut h = Vec::new();
        let mut ix = Vec::new();
        for (i, ch) in chars.iter().enumerate() {
            for c in fold_char(*ch).chars() {
                h.push(c);
                ix.push(i);
            }
        }
        (h, ix)
    };
    let nd: Vec<char> = needle.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + nd.len() <= hay.len() {
        if hay[i..i + nd.len()] == nd[..] {
            out.push((idx[i], idx[i + nd.len() - 1] + 1));
            i += nd.len();
        } else {
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// WebKit SerializedScriptValue / IDB key parsing (just enough for message records)
// ---------------------------------------------------------------------------------------------

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    memchr::memmem::find(&hay[from..], needle).map(|p| p + from)
}

fn read_ssv_string(buf: &[u8], pos: usize) -> Option<String> {
    let hdr = buf.get(pos..pos + 4)?;
    let n = u32::from_le_bytes(hdr.try_into().ok()?);
    let start = pos + 4;
    if n & 0x8000_0000 != 0 {
        let len = (n & 0x7fff_ffff) as usize;
        let b = buf.get(start..start.checked_add(len)?)?;
        Some(b.iter().map(|&c| c as char).collect())
    } else {
        let len = n as usize;
        let b = buf.get(start..start.checked_add(len.checked_mul(2)?)?)?;
        let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&units))
    }
}

fn field_str(buf: &[u8], name: &str) -> Option<String> {
    let mut key = ((name.len() as u32) | 0x8000_0000).to_le_bytes().to_vec();
    key.extend_from_slice(name.as_bytes());
    let mut from = 0;
    while let Some(i) = find(buf, &key, from) {
        let p = i + key.len();
        if buf.get(p) == Some(&0x10) {
            if let Some(s) = read_ssv_string(buf, p + 1) {
                return Some(s);
            }
        }
        from = i + 1;
    }
    None
}

fn field_f64(buf: &[u8], name: &str) -> Option<f64> {
    let mut key = ((name.len() as u32) | 0x8000_0000).to_le_bytes().to_vec();
    key.extend_from_slice(name.as_bytes());
    let mut from = 0;
    while let Some(i) = find(buf, &key, from) {
        let p = i + key.len();
        if buf.get(p) == Some(&0x0a) {
            if let Some(b) = buf.get(p + 1..p + 9) {
                return Some(f64::from_le_bytes(b.try_into().ok()?));
            }
        }
        from = i + 1;
    }
    None
}

/// Parses an IDB array key made of strings (`0x60`, u32 length, UTF-16LE).
fn key_strings(k: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < k.len() {
        if k[i] == 0x60 && i + 5 <= k.len() {
            let n = u32::from_le_bytes([k[i + 1], k[i + 2], k[i + 3], k[i + 4]]) as usize;
            let end = i + 5 + n * 2;
            if end <= k.len() {
                let units: Vec<u16> = k[i + 5..end]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                out.push(String::from_utf16_lossy(&units));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Search engine
// ---------------------------------------------------------------------------------------------

fn indexeddb_root(webkit_dir: &Path) -> PathBuf {
    webkit_dir
        .join("zalo-data")
        .join("databases")
        .join("indexeddb")
        .join("v1")
        .join("zalo-app_local_0")
}

/// A scanned message record; `cipher` holds the UTF-16 units of Zalo's XOR-"BasicEncryption"
/// `message` field (used by ~90% of messages) that is decrypted once the key is recovered.
struct RawRec {
    rid: i64,
    ts_ms: f64,
    sender: String,
    uid: String,
    text: String,
    attach: String,
    cipher: Option<Vec<u16>>,
}

fn utf16_of(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Zalo's local "BasicEncryption" is `charCode ^ key[i % key.len()]` with a per-account secret that
/// is not stored in plain form. The stored `message` is `JSON.stringify(value)` XOR-ed with that key,
/// so we recover it by letter-frequency analysis against this account's own plaintext messages
/// (`corpus`, counts of chars < 256), then verify that decrypted samples parse as JSON.
fn recover_key(ciphers: &[&Vec<u16>], corpus: &[u64; 256]) -> Option<Vec<u8>> {
    if ciphers.is_empty() {
        return None;
    }
    let total: u64 = corpus.iter().sum();
    if total < 1000 {
        return None;
    }
    let table: Vec<f64> = corpus
        .iter()
        .map(|&n| ((n as f64 + 0.5) / (total as f64 + 35000.0)).ln())
        .collect();

    let sample: Vec<&&Vec<u16>> = ciphers.iter().take(30_000).collect();
    let mut best: Option<(f64, Vec<u8>)> = None;
    for &len in &[32usize, 16, 64, 24] {
        let mut key = Vec::with_capacity(len);
        for j in 0..len {
            let mut counts = [0u64; 256];
            for c in &sample {
                let mut i = j;
                while i < c.len() {
                    if c[i] < 256 {
                        counts[c[i] as usize] += 1;
                    }
                    i += len;
                }
            }
            let mut best_kb = (f64::MIN, 32u8);
            for kb in 32u8..127 {
                let score: f64 = (0..256usize).map(|ch| counts[ch] as f64 * table[ch ^ kb as usize]).sum();
                if score > best_kb.0 {
                    best_kb = (score, kb);
                }
            }
            key.push(best_kb.1);
        }
        let ok = ciphers
            .iter()
            .take(400)
            .filter(|c| serde_json::from_str::<serde_json::Value>(&xor_decode(c, &key)).is_ok())
            .count() as f64
            / ciphers.len().min(400) as f64;
        if best.as_ref().map(|b| ok > b.0).unwrap_or(true) {
            best = Some((ok, key));
        }
        if ok > 0.9 {
            break;
        }
    }
    best.filter(|(ok, _)| *ok > 0.6).map(|(_, k)| k)
}

fn xor_decode(units: &[u16], key: &[u8]) -> String {
    let out: Vec<u16> = units.iter().enumerate().map(|(i, u)| u ^ key[i % key.len()] as u16).collect();
    String::from_utf16_lossy(&out)
}

type ParsedChunk = (Vec<RawRec>, [u64; 256], Vec<(String, String)>);

/// Parses every `message` record whose `recordID % parts == part` (one slice per worker thread).
fn parse_slice(
    db: &Path,
    sid: i64,
    parts: i64,
    part: i64,
    sent_at: &HashMap<i64, f64>,
) -> Result<ParsedChunk> {
    let conn = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut st = conn.prepare(
        "SELECT recordID, value FROM Records WHERE objectStoreID = ?1 AND recordID % ?2 = ?3",
    )?;
    let mut rows = st.query([sid, parts, part])?;
    let mut recs = Vec::new();
    let mut corpus = [0u64; 256];
    let mut names = Vec::new();
    while let Some(r) = rows.next()? {
        let rid: i64 = r.get(0)?;
        let val = match r.get_ref(1)?.as_bytes() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let msg = field_str(val, "msg");
        let enc = if msg.is_none() { field_str(val, "message") } else { None };
        if let Some(m) = &msg {
            for ch in m.chars() {
                if (ch as u32) < 256 {
                    corpus[ch as usize] += 1;
                }
            }
            corpus[b'"' as usize] += 2; // JSON.stringify quotes
        }
        let name = field_str(val, "fromD").or_else(|| field_str(val, "dName")).unwrap_or_default();
        let uid = field_str(val, "uid")
            .or_else(|| field_str(val, "fromUid"))
            .filter(|u| u != "0" && !u.is_empty())
            .unwrap_or_default();
        if !name.is_empty() && !uid.is_empty() {
            names.push((uid.clone(), name.clone()));
        }
        recs.push(RawRec {
            rid,
            ts_ms: field_f64(val, "ts")
                .filter(|t| *t > 0.0)
                .or_else(|| sent_at.get(&rid).copied())
                .unwrap_or(0.0),
            sender: name,
            uid,
            text: msg.unwrap_or_default(),
            attach: field_str(val, "attach").unwrap_or_default(),
            cipher: enc.as_deref().map(utf16_of),
        });
    }
    Ok((recs, corpus, names))
}

struct Matcher<'a> {
    needle: &'a str,
    exact: bool,
    sender_f: &'a str,
    key: Option<&'a [u8]>,
    /// needles containing JSON-escaped characters must be matched on the parsed value
    parse_first: bool,
    names: &'a HashMap<String, String>,
    conv: &'a HashMap<i64, String>,
}

impl Matcher<'_> {
    fn contains(&self, s: &str) -> bool {
        if self.exact {
            s.contains(self.needle)
        } else {
            fold(s).contains(self.needle)
        }
    }

    fn process(&self, recs: &[RawRec]) -> Vec<Hit> {
        let mut hits = Vec::new();
        for rec in recs {
            let mut text = rec.text.clone();
            let mut attach = rec.attach.clone();
            if let (Some(c), Some(k)) = (rec.cipher.as_ref(), self.key) {
                let dec = xor_decode(c, k);
                if !self.parse_first && !self.contains(&dec) && !self.contains(&attach) {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(&dec) {
                    Ok(serde_json::Value::String(s)) => text = s,
                    Ok(_) => {
                        if attach.is_empty() {
                            attach = dec;
                        }
                    }
                    Err(_) => text = dec,
                }
            }
            if !(self.contains(&text) || self.contains(&attach)) {
                continue;
            }
            let sender_name = if !rec.sender.is_empty() {
                rec.sender.clone()
            } else {
                self.names.get(&rec.uid).cloned().unwrap_or_else(|| rec.uid.clone())
            };
            if !self.sender_f.is_empty() && !fold(&sender_name).contains(self.sender_f) {
                continue;
            }
            hits.push(Hit {
                ts_ms: rec.ts_ms,
                conv_id: self.conv.get(&rec.rid).cloned().unwrap_or_default(),
                sender: sender_name,
                text,
                attach,
            });
        }
        hits
    }
}

/// Opens the live IndexedDB file read-only (no copy). Falls back to a temporary copy if that fails.
fn open_source(src: &Path, tmp: &Path) -> Result<(rusqlite::Connection, PathBuf)> {
    let force_copy = std::env::var_os("HPROZALO_SEARCH_COPY").is_some();
    if let Ok(c) = rusqlite::Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e).and_then(|c| if force_copy { Err(rusqlite::Error::InvalidQuery) } else { Ok(c) }) {
        if c.query_row("SELECT count(*) FROM ObjectStoreInfo", [], |r| r.get::<_, i64>(0)).is_ok() {
            return Ok((c, src.to_path_buf()));
        }
    }
    let dst = tmp.join("IndexedDB.sqlite3");
    for suf in ["", "-wal", "-shm"] {
        let s = PathBuf::from(format!("{}{}", src.display(), suf));
        if s.exists() {
            std::fs::copy(&s, PathBuf::from(format!("{}{}", dst.display(), suf)))?;
        }
    }
    let c = rusqlite::Connection::open_with_flags(&dst, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok((c, dst))
}

pub fn search(
    root: &Path,
    keyword: &str,
    sender: &str,
    exact: bool,
    progress: &dyn Fn(&str),
) -> Result<(usize, Vec<Hit>)> {
    let needle = if exact { keyword.to_string() } else { fold(keyword) };
    let sender_f = fold(sender);
    let parse_first = needle.chars().any(|c| c == '"' || c == '\\' || c.is_control());
    let tmp = tempfile::Builder::new().prefix("hprozalo-search-").tempdir()?;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 8) as i64;
    let mut hits = Vec::new();
    let mut scanned = 0usize;
    let timing = std::env::var_os("HPROZALO_TIMING").is_some();

    let entries = std::fs::read_dir(root)
        .with_context(|| format!("không đọc được thư mục dữ liệu {}", root.display()))?;
    for entry in entries.flatten() {
        let src = entry.path().join("IndexedDB.sqlite3");
        if !src.is_file() {
            continue;
        }
        let t0 = std::time::Instant::now();
        let work_dir = tmp.path().join(entry.file_name());
        std::fs::create_dir_all(&work_dir)?;
        let (conn, db_path) = match open_source(&src, &work_dir) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let sid: i64 = match conn.query_row(
            "SELECT id FROM ObjectStoreInfo WHERE name = 'message'",
            [],
            |r| r.get(0),
        ) {
            Ok(v) => v,
            Err(_) => continue,
        };
        scanned += 1;
        progress("Đang đọc tin nhắn…");

        let mut conv: HashMap<i64, String> = HashMap::new();
        let mut sent_at: HashMap<i64, f64> = HashMap::new();
        {
            let mut st = conn.prepare(
                "SELECT objectStoreRecordID, key FROM IndexRecords WHERE indexID = ?1 AND objectStoreID = ?2",
            )?;
            let mut rows = st.query([INDEX_USER_SENDDTTM_MSGID, sid])?;
            while let Some(r) = rows.next()? {
                let rid: i64 = r.get(0)?;
                let k = r.get_ref(1)?.as_bytes().unwrap_or(&[]);
                let parts = key_strings(k);
                if let Some(first) = parts.first() {
                    conv.insert(rid, first.clone());
                }
                if let Some(t) = parts.get(1).and_then(|t| t.parse::<f64>().ok()) {
                    sent_at.insert(rid, t);
                }
            }
        }
        if timing {
            eprintln!("[t] index {:?}", t0.elapsed());
        }

        // Pass 1 (parallel): parse every record; collect plaintext corpus for key recovery.
        let chunks: Vec<Result<ParsedChunk>> = std::thread::scope(|sc| {
            let handles: Vec<_> = (0..threads)
                .map(|part| {
                    let (db, sa) = (&db_path, &sent_at);
                    sc.spawn(move || parse_slice(db, sid, threads, part, sa))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("luồng đọc bị lỗi"))))
                .collect()
        });
        let mut recs: Vec<RawRec> = Vec::new();
        let mut corpus = [0u64; 256];
        let mut names: HashMap<String, String> = HashMap::new();
        for c in chunks {
            let (r, co, nm) = c?;
            recs.extend(r);
            for (i, n) in co.iter().enumerate() {
                corpus[i] += n;
            }
            for (u, n) in nm {
                names.entry(u).or_insert(n);
            }
        }
        if timing {
            eprintln!("[t] parse {:?} ({} recs)", t0.elapsed(), recs.len());
        }

        progress("Đang giải mã tin nhắn…");
        let key = {
            let ciphers: Vec<&Vec<u16>> = recs.iter().filter_map(|r| r.cipher.as_ref()).collect();
            recover_key(&ciphers, &corpus)
        };
        if timing {
            eprintln!("[t] key {:?} = {:?}", t0.elapsed(), key.as_ref().map(|k| String::from_utf8_lossy(k).to_string()));
        }

        // Pass 2 (parallel): decrypt + match.
        let matcher = Matcher {
            needle: &needle,
            exact,
            sender_f: &sender_f,
            key: key.as_deref(),
            parse_first,
            names: &names,
            conv: &conv,
        };
        let chunk_len = recs.len().div_ceil(threads as usize).max(1);
        let found: Vec<Vec<Hit>> = std::thread::scope(|sc| {
            let handles: Vec<_> = recs
                .chunks(chunk_len)
                .map(|c| {
                    let m = &matcher;
                    sc.spawn(move || m.process(c))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        for f in found {
            hits.extend(f);
        }
        if timing {
            eprintln!("[t] done {:?}", t0.elapsed());
        }
    }
    hits.sort_by(|a, b| b.ts_ms.partial_cmp(&a.ts_ms).unwrap_or(std::cmp::Ordering::Equal));
    Ok((scanned, hits))
}

// ---------------------------------------------------------------------------------------------
// GTK window
// ---------------------------------------------------------------------------------------------

fn when_str(ts_ms: f64) -> String {
    if ts_ms <= 0.0 {
        return "?".to_string();
    }
    glib::DateTime::from_unix_local((ts_ms / 1000.0) as i64)
        .ok()
        .and_then(|d| d.format("%d/%m/%Y %H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn escape_markup(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

fn preview_markup(h: &Hit, needle: &str, exact: bool) -> String {
    let mut base: String = h.text.replace('\n', " ");
    if base.is_empty() && !h.attach.is_empty() {
        let title = serde_json::from_str::<serde_json::Value>(&h.attach)
            .ok()
            .and_then(|v| v.get("title").and_then(|t| t.as_str().map(str::to_string)))
            .unwrap_or_else(|| "tệp đính kèm".to_string());
        base = format!("📎 {title}");
    }
    let chars: Vec<char> = base.chars().take(300).collect();
    let base: String = chars.iter().collect();
    let mut out = String::new();
    let mut last = 0usize;
    for (a, b) in match_ranges(&base, needle, exact) {
        out.push_str(&escape_markup(&chars[last..a].iter().collect::<String>()));
        out.push_str(&format!(
            "<span background=\"#fde68a\" weight=\"bold\">{}</span>",
            escape_markup(&chars[a..b].iter().collect::<String>())
        ));
        last = b;
    }
    out.push_str(&escape_markup(&chars[last..].iter().collect::<String>()));
    out
}

fn human_size(bytes: f64) -> String {
    let mut n = bytes;
    for unit in ["B", "KB", "MB", "GB"] {
        if n < 1024.0 || unit == "GB" {
            return if unit == "B" { format!("{n:.0} {unit}") } else { format!("{n:.1} {unit}") };
        }
        n /= 1024.0;
    }
    String::new()
}

fn url_ranges(text: &str) -> Vec<(i32, i32, String)> {
    // char-offset ranges of http(s) URLs in `text`
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..chars.len().min(i + 8)].iter().collect();
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let mut j = i;
            while j < chars.len() && !chars[j].is_whitespace() && !matches!(chars[j], '<' | '>' | '"' | '\'') {
                j += 1;
            }
            out.push((i as i32, j as i32, chars[i..j].iter().collect()));
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

struct Ui {
    window: gtk::Window,
    query: gtk::SearchEntry,
    sender: gtk::Entry,
    exact: gtk::CheckButton,
    search_btn: gtk::Button,
    store: gtk::ListStore,
    tree: gtk::TreeView,
    detail: gtk::TextView,
    status: gtk::Label,
    hits: RefCell<Vec<Hit>>,
    needle: RefCell<String>,
    exact_mode: RefCell<bool>,
    links: RefCell<Vec<(i32, i32, String)>>,
    busy: RefCell<bool>,
    root: PathBuf,
}

fn put(buf: &gtk::TextBuffer, text: &str, tags: &[&gtk::TextTag]) {
    let start = buf.end_iter().offset();
    let mut end = buf.end_iter();
    buf.insert(&mut end, text);
    let s = buf.iter_at_offset(start);
    let e = buf.end_iter();
    for t in tags {
        buf.apply_tag(*t, &s, &e);
    }
}

impl Ui {
    fn current_hit(&self) -> Option<Hit> {
        let (model, iter) = self.tree.selection().selected()?;
        let idx: u32 = model.value(&iter, 4).get().ok()?;
        self.hits.borrow().get(idx as usize).cloned()
    }

    fn show_detail(&self) {
        let Some(h) = self.current_hit() else { return };
        let buf = self.detail.buffer().expect("buffer");
        buf.set_text("");
        self.links.borrow_mut().clear();
        let t_sender = buf.tag_table().and_then(|t| t.lookup("sender")).expect("t");
        let t_meta = buf.tag_table().and_then(|t| t.lookup("meta")).expect("t");
        let t_body = buf.tag_table().and_then(|t| t.lookup("body")).expect("t");
        let t_hit = buf.tag_table().and_then(|t| t.lookup("hit")).expect("t");
        let t_link = buf.tag_table().and_then(|t| t.lookup("link")).expect("t");
        let t_att_h = buf.tag_table().and_then(|t| t.lookup("att_h")).expect("t");
        let t_att = buf.tag_table().and_then(|t| t.lookup("att")).expect("t");

        let who = if h.sender.is_empty() { "(không rõ)" } else { &h.sender };
        put(&buf, who, &[&t_sender]);
        put(&buf, &format!("   {}\n", when_str(h.ts_ms)), &[&t_meta]);
        put(&buf, &format!("Cuộc trò chuyện: {}\n\n", if h.conv_id.is_empty() { "(không rõ)" } else { &h.conv_id }), &[&t_meta]);

        let start = buf.end_iter().offset();
        let body = if h.text.is_empty() { "(không có nội dung chữ)" } else { &h.text };
        put(&buf, body, &[&t_body]);
        let exact = *self.exact_mode.borrow();
        let needle = self.needle.borrow().clone();
        for (a, b) in match_ranges(&h.text, &needle, exact) {
            buf.apply_tag(&t_hit, &buf.iter_at_offset(start + a as i32), &buf.iter_at_offset(start + b as i32));
        }
        for (a, b, url) in url_ranges(&h.text) {
            buf.apply_tag(&t_link, &buf.iter_at_offset(start + a), &buf.iter_at_offset(start + b));
            self.links.borrow_mut().push((start + a, start + b, url));
        }

        if !h.attach.is_empty() {
            put(&buf, "\n\n📎 Đính kèm\n", &[&t_att_h]);
            match serde_json::from_str::<serde_json::Value>(&h.attach).map(|v| match v {
                serde_json::Value::Array(a) if !a.is_empty() => a.into_iter().next().unwrap(),
                other => other,
            }) {
                Ok(v) => {
                    if let Some(t) = v.get("title").and_then(|x| x.as_str()) {
                        put(&buf, &format!("{t}\n"), &[&t_att]);
                    }
                    let size = v
                        .get("params")
                        .and_then(|p| p.as_str())
                        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                        .and_then(|p| p.get("fileSize").and_then(|s| s.as_str().and_then(|s| s.parse::<f64>().ok())));
                    if let Some(sz) = size {
                        put(&buf, &format!("Dung lượng: {}\n", human_size(sz)), &[&t_att]);
                    }
                    if let Some(d) = v.get("description").and_then(|x| x.as_str()).filter(|d| !d.is_empty()) {
                        put(&buf, &format!("{d}\n"), &[&t_att]);
                    }
                    if let Some(href) = v.get("href").and_then(|x| x.as_str()) {
                        let s2 = buf.end_iter().offset();
                        put(&buf, href, &[&t_link]);
                        self.links.borrow_mut().push((s2, s2 + href.chars().count() as i32, href.to_string()));
                    }
                }
                Err(_) => put(&buf, &h.attach.chars().take(600).collect::<String>(), &[&t_att]),
            }
        }
    }

    fn url_at(&self, x: f64, y: f64) -> Option<String> {
        let (bx, by) = self.detail.window_to_buffer_coords(gtk::TextWindowType::Text, x as i32, y as i32);
        let iter = self.detail.iter_at_location(bx, by)?;
        let off = iter.offset();
        self.links.borrow().iter().find(|(a, b, _)| *a <= off && off < *b).map(|(_, _, u)| u.clone())
    }

    fn open_in_zalo(&self) {
        let Some(h) = self.current_hit() else {
            self.status.set_text("Chọn một dòng kết quả trước.");
            return;
        };
        if h.conv_id.is_empty() || !h.conv_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            self.status.set_text("Không xác định được cuộc trò chuyện để mở.");
            return;
        }
        let spawn = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .arg(format!("zalo://conversation/{}", h.conv_id))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
        });
        match spawn {
            Ok(_) => self.status.set_text(&format!(
                "Đã mở cuộc trò chuyện trong Zalo — hãy cuộn tới tin lúc {}.",
                when_str(h.ts_ms)
            )),
            Err(e) => self.status.set_text(&format!("Lỗi mở Zalo: {e}")),
        }
    }

    fn start_search(self: &Rc<Self>) {
        let kw = self.query.text().trim().to_string();
        if kw.is_empty() || *self.busy.borrow() {
            return;
        }
        *self.busy.borrow_mut() = true;
        self.search_btn.set_sensitive(false);
        self.store.clear();
        if let Some(b) = self.detail.buffer() {
            b.set_text("");
        }
        self.status.set_text("Đang tìm…");
        let exact = self.exact.is_active();
        let sender = self.sender.text().trim().to_string();
        *self.exact_mode.borrow_mut() = exact;
        *self.needle.borrow_mut() = if exact { kw.clone() } else { fold(&kw) };

        let (tx, rx) = mpsc::channel::<Result<(usize, Vec<Hit>), String>>();
        let (ptx, prx) = mpsc::channel::<String>();
        let root = self.root.clone();
        std::thread::spawn(move || {
            let res = search(&root, &kw, &sender, exact, &|m| {
                let _ = ptx.send(m.to_string());
            })
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });

        let ui = Rc::clone(self);
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            while let Ok(m) = prx.try_recv() {
                ui.status.set_text(&m);
            }
            match rx.try_recv() {
                Ok(res) => {
                    ui.finish_search(res);
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    ui.finish_search(Err("luồng tìm kiếm bị dừng bất ngờ".into()));
                    glib::ControlFlow::Break
                }
            }
        });
    }

    fn finish_search(&self, res: Result<(usize, Vec<Hit>), String>) {
        *self.busy.borrow_mut() = false;
        self.search_btn.set_sensitive(true);
        match res {
            Err(e) => self.status.set_text(&format!("Lỗi: {e}")),
            Ok((_scanned, hits)) => {
                let needle = self.needle.borrow().clone();
                let exact = *self.exact_mode.borrow();
                for (i, h) in hits.iter().take(MAX_ROWS).enumerate() {
                    self.store.set(
                        &self.store.append(),
                        &[
                            (0, &when_str(h.ts_ms)),
                            (1, &h.sender),
                            (2, &h.conv_id),
                            (3, &preview_markup(h, &needle, exact)),
                            (4, &(i as u32)),
                        ],
                    );
                }
                let extra = if hits.len() > MAX_ROWS { format!(" (hiển thị {MAX_ROWS} đầu)") } else { String::new() };
                self.status.set_text(&format!(
                    "{} kết quả{extra}. Nhấp đúp một dòng để mở cuộc trò chuyện trong Zalo.",
                    hits.len()
                ));
                *self.hits.borrow_mut() = hits;
            }
        }
    }
}

pub fn run_cli(webkit_dir: &Path, keyword: &str) -> Result<()> {
    let (scanned, hits) = search(&indexeddb_root(webkit_dir), keyword, "", false, &|_| {})?;
    println!("Quét {scanned} database — {} kết quả cho '{keyword}'", hits.len());
    for h in hits.iter().take(15) {
        let t = if h.text.is_empty() { h.attach.chars().take(160).collect::<String>() } else { h.text.chars().take(160).collect() };
        println!("[{}] {} | {} | {}", when_str_plain(h.ts_ms), h.conv_id, h.sender, t.replace('\n', " "));
    }
    Ok(())
}

fn when_str_plain(ts_ms: f64) -> String {
    when_str(ts_ms)
}

pub fn run(webkit_dir: &Path) -> Result<()> {
    gtk::init().context("không khởi tạo được GTK")?;

    let provider = gtk::CssProvider::new();
    let _ = provider.load_from_data(b".kv-detail { background: #fbfbfc; }");
    if let Some(screen) = gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(&screen, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Tìm tin nhắn cũ — HPro Zalo");
    window.set_default_size(1020, 680);
    window.connect_destroy(|_| gtk::main_quit());

    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root_box.set_border_width(10);
    window.add(&root_box);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let query = gtk::SearchEntry::new();
    query.set_placeholder_text(Some("Từ khoá (không phân biệt hoa/thường, dấu)"));
    let sender = gtk::Entry::new();
    sender.set_placeholder_text(Some("Người gửi (tuỳ chọn)"));
    sender.set_width_chars(20);
    let exact = gtk::CheckButton::with_label("Khớp chính xác");
    let search_btn = gtk::Button::with_label("Tìm");
    search_btn.style_context().add_class("suggested-action");
    bar.pack_start(&query, true, true, 0);
    bar.pack_start(&sender, false, false, 0);
    bar.pack_start(&exact, false, false, 0);
    bar.pack_start(&search_btn, false, false, 0);
    root_box.pack_start(&bar, false, false, 0);

    let paned = gtk::Paned::new(gtk::Orientation::Vertical);
    root_box.pack_start(&paned, true, true, 0);

    let store = gtk::ListStore::new(&[
        String::static_type(),
        String::static_type(),
        String::static_type(),
        String::static_type(),
        u32::static_type(),
    ]);
    let tree = gtk::TreeView::with_model(&store);
    for (i, (title, width, markup)) in [("Thời gian", 130, false), ("Người gửi", 150, false), ("Cuộc trò chuyện", 170, false), ("Nội dung", 420, true)]
        .into_iter()
        .enumerate()
    {
        let renderer = gtk::CellRendererText::new();
        renderer.set_ellipsize(pango::EllipsizeMode::End);
        let col = gtk::TreeViewColumn::new();
        col.set_title(title);
        gtk::prelude::CellLayoutExt::pack_start(&col, &renderer, true);
        gtk::prelude::CellLayoutExt::add_attribute(&col, &renderer, if markup { "markup" } else { "text" }, i as i32);
        col.set_resizable(true);
        col.set_min_width(width);
        if !markup {
            col.set_sort_column_id(i as i32);
        }
        tree.append_column(&col);
    }
    let sw = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
    sw.add(&tree);
    paned.pack1(&sw, true, false);

    let detail = gtk::TextView::new();
    detail.set_editable(false);
    detail.set_cursor_visible(false);
    detail.set_wrap_mode(gtk::WrapMode::WordChar);
    detail.style_context().add_class("kv-detail");
    detail.set_left_margin(18);
    detail.set_right_margin(18);
    detail.set_top_margin(14);
    detail.set_bottom_margin(14);
    {
        let buf = detail.buffer().expect("buffer");
        let table = buf.tag_table().expect("tag table");
        let bold = 700;
        table.add(&gtk::TextTag::builder().name("sender").weight(bold).scale(1.2).build());
        table.add(&gtk::TextTag::builder().name("meta").foreground("#6b7280").scale(0.9).build());
        table.add(&gtk::TextTag::builder().name("body").scale(1.12).pixels_above_lines(6).pixels_inside_wrap(4).build());
        table.add(&gtk::TextTag::builder().name("hit").background("#fde68a").weight(bold).build());
        table.add(&gtk::TextTag::builder().name("link").foreground("#2563eb").underline(pango::Underline::Single).build());
        table.add(&gtk::TextTag::builder().name("att_h").weight(bold).foreground("#374151").pixels_above_lines(12).build());
        table.add(&gtk::TextTag::builder().name("att").foreground("#374151").build());
    }
    let sw2 = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
    sw2.add(&detail);
    paned.pack2(&sw2, false, false);
    paned.set_position(380);

    let foot = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let status = gtk::Label::new(Some("Nhập từ khoá rồi bấm Tìm. Nhấp đúp một dòng để mở cuộc trò chuyện trong Zalo."));
    status.set_xalign(0.0);
    status.set_ellipsize(pango::EllipsizeMode::End);
    let open_btn = gtk::Button::with_label("Mở trong Zalo");
    let copy_btn = gtk::Button::with_label("Sao chép tin nhắn");
    foot.pack_start(&status, true, true, 0);
    foot.pack_start(&open_btn, false, false, 0);
    foot.pack_start(&copy_btn, false, false, 0);
    root_box.pack_start(&foot, false, false, 0);

    let ui = Rc::new(Ui {
        window: window.clone(),
        query: query.clone(),
        sender: sender.clone(),
        exact: exact.clone(),
        search_btn: search_btn.clone(),
        store,
        tree: tree.clone(),
        detail: detail.clone(),
        status,
        hits: RefCell::new(Vec::new()),
        needle: RefCell::new(String::new()),
        exact_mode: RefCell::new(false),
        links: RefCell::new(Vec::new()),
        busy: RefCell::new(false),
        root: indexeddb_root(webkit_dir),
    });

    { let u = Rc::clone(&ui); query.connect_activate(move |_| u.start_search()); }
    { let u = Rc::clone(&ui); sender.connect_activate(move |_| u.start_search()); }
    { let u = Rc::clone(&ui); search_btn.connect_clicked(move |_| u.start_search()); }
    { let u = Rc::clone(&ui); tree.selection().connect_changed(move |_| u.show_detail()); }
    { let u = Rc::clone(&ui); tree.connect_row_activated(move |_, _, _| u.open_in_zalo()); }
    { let u = Rc::clone(&ui); open_btn.connect_clicked(move |_| u.open_in_zalo()); }
    {
        let u = Rc::clone(&ui);
        copy_btn.connect_clicked(move |_| {
            if let Some(h) = u.current_hit() {
                let text = if h.text.is_empty() { h.attach } else { h.text };
                gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD).set_text(&text);
                u.status.set_text("Đã sao chép nội dung tin nhắn.");
            }
        });
    }
    {
        let u = Rc::clone(&ui);
        detail.connect_button_release_event(move |tv, ev| {
            let has_sel = tv.buffer().map(|b| b.has_selection()).unwrap_or(false);
            if !has_sel {
                let (x, y) = ev.position();
                if let Some(url) = u.url_at(x, y) {
                    let _ = gtk::show_uri_on_window(Some(&u.window), &url, gtk::current_event_time());
                }
            }
            glib::Propagation::Proceed
        });
    }
    {
        let u = Rc::clone(&ui);
        detail.connect_motion_notify_event(move |tv, ev| {
            let (x, y) = ev.position();
            if let Some(win) = TextViewExt::window(tv, gtk::TextWindowType::Text) {
                let name = if u.url_at(x, y).is_some() { "pointer" } else { "text" };
                win.set_cursor(gdk::Cursor::from_name(&win.display(), name).as_ref());
            }
            glib::Propagation::Proceed
        });
    }

    window.show_all();
    query.grab_focus();
    gtk::main();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_strips_vietnamese_diacritics_and_case() {
        assert_eq!(fold("Khai BÁO Đường"), "khai bao duong");
    }

    #[test]
    fn match_ranges_maps_back_to_original_chars() {
        let r = match_ranges("Nhờ khai báo Google", "khai bao", false);
        assert_eq!(r, vec![(4, 12)]);
    }

    #[test]
    fn parses_latin1_and_utf16_ssv_strings() {
        let mut buf = Vec::new();
        buf.extend(((3u32) | 0x8000_0000).to_le_bytes());
        buf.extend(b"msg");
        buf.push(0x10);
        buf.extend(((2u32) | 0x8000_0000).to_le_bytes());
        buf.extend(b"hi");
        assert_eq!(field_str(&buf, "msg").as_deref(), Some("hi"));

        let mut b2 = Vec::new();
        b2.extend(((3u32) | 0x8000_0000).to_le_bytes());
        b2.extend(b"msg");
        b2.push(0x10);
        b2.extend(2u32.to_le_bytes());
        for u in "ễ!".encode_utf16() {
            b2.extend(u.to_le_bytes());
        }
        assert_eq!(field_str(&b2, "msg").as_deref(), Some("ễ!"));
    }

    #[test]
    fn parses_index_key_strings() {
        let mut k = vec![0u8];
        for s in ["g1", "42"] {
            k.push(0x60);
            k.extend((s.len() as u32).to_le_bytes());
            for u in s.encode_utf16() {
                k.extend(u.to_le_bytes());
            }
        }
        assert_eq!(key_strings(&k), vec!["g1".to_string(), "42".to_string()]);
    }
}
