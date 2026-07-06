//! Versioned JSON persistence for notes: hand-rolled serializer + parser
//! (no serde), atomic writes via a sibling temp file + rename.

use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct NoteData {
    pub id: u64,
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub free: bool,     // floating vs stacked (future phases)
    pub docked: i8,     // 0=none, -1=left, 1=right (future)
    pub color: u8,      // palette index (future)
    pub font_size: f32, // (future)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Store {
    pub version: u32,
    pub next_id: u64,
    pub notes: Vec<NoteData>,
}

impl Default for Store {
    fn default() -> Self {
        Store {
            version: 1,
            next_id: 1,
            notes: Vec::new(),
        }
    }
}

/// `%APPDATA%\liquidnotes` (or a temp-dir fallback). Created on demand.
pub fn store_dir() -> PathBuf {
    let dir = match std::env::var_os("APPDATA") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("liquidnotes"),
        _ => std::env::temp_dir().join("liquidnotes"),
    };
    // Ignore AlreadyExists (and any other error — save_atomic will surface it).
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn store_path() -> PathBuf {
    store_dir().join("notes.json")
}

/// Load the store from disk. Any failure (missing file, bad JSON, wrong
/// types) yields `Store::default()` — never panics.
pub fn load() -> Store {
    load_from(&store_path())
}

pub(crate) fn load_from(path: &Path) -> Store {
    match std::fs::read_to_string(path) {
        Ok(s) => parse(&s).unwrap_or_default(),
        Err(_) => Store::default(),
    }
}

/// Serialize and write atomically: sibling temp file in the same directory
/// (so the rename never crosses volumes), flush, then rename over the final
/// path — on Windows `std::fs::rename` replaces the destination atomically.
pub fn save_atomic(store: &Store) -> std::io::Result<()> {
    save_atomic_to(&store_path(), store)
}

pub(crate) fn save_atomic_to(path: &Path, store: &Store) -> std::io::Result<()> {
    let json = to_json(store);
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.flush()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

// ---------------------------------------------------------------- serialize

pub(crate) fn to_json(store: &Store) -> String {
    let mut s = String::with_capacity(64 + store.notes.len() * 128);
    s.push_str(&format!(
        "{{\"version\":{},\"next_id\":{},\"notes\":[",
        store.version, store.next_id
    ));
    for (i, n) in store.notes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"id\":{},\"text\":\"{}\",\"x\":{},\"y\":{},\"w\":{},\"h\":{},\
             \"free\":{},\"docked\":{},\"color\":{},\"font_size\":{}}}",
            n.id,
            escape(&n.text),
            n.x,
            n.y,
            n.w,
            n.h,
            n.free,
            n.docked,
            n.color,
            fmt_f32(n.font_size)
        ));
    }
    s.push_str("]}");
    s
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// f32 with enough precision to round-trip; `{:?}` always keeps a decimal
/// point (e.g. `16.0`) and prints the shortest exact representation.
fn fmt_f32(v: f32) -> String {
    if v.is_finite() {
        format!("{:?}", v)
    } else {
        "0.0".to_string()
    }
}

// ------------------------------------------------------------------- parse

#[derive(Debug)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get<'a>(&'a self, key: &str) -> Option<&'a Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn num(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
    fn int(&self) -> Option<i64> {
        let n = self.num()?;
        if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
            Some(n as i64)
        } else {
            None
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Parser { b: s.as_bytes(), i: 0 }
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect_lit(&mut self, lit: &str) -> Option<()> {
        if self.b[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Some(())
        } else {
            None
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.skip_ws();
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Some(Json::Str(self.string()?)),
            b't' => {
                self.expect_lit("true")?;
                Some(Json::Bool(true))
            }
            b'f' => {
                self.expect_lit("false")?;
                Some(Json::Bool(false))
            }
            b'n' => {
                self.expect_lit("null")?;
                Some(Json::Null)
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn object(&mut self) -> Option<Json> {
        if !self.eat(b'{') {
            return None;
        }
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.eat(b'}') {
            return Some(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if !self.eat(b':') {
                return None;
            }
            let val = self.value()?;
            pairs.push((key, val));
            self.skip_ws();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                return Some(Json::Obj(pairs));
            }
            return None;
        }
    }

    fn array(&mut self) -> Option<Json> {
        if !self.eat(b'[') {
            return None;
        }
        let mut items = Vec::new();
        self.skip_ws();
        if self.eat(b']') {
            return Some(Json::Arr(items));
        }
        loop {
            let val = self.value()?;
            items.push(val);
            self.skip_ws();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b']') {
                return Some(Json::Arr(items));
            }
            return None;
        }
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            match c {
                b'"' => {
                    self.i += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.i += 1;
                    let e = self.peek()?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                // Surrogate pair: expect \uDC00..\uDFFF next.
                                if !(self.eat(b'\\') && self.eat(b'u')) {
                                    return None;
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return None;
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp)?);
                        }
                        _ => return None,
                    }
                }
                _ if c < 0x20 => return None, // raw control char: invalid
                _ => {
                    // Copy one UTF-8 encoded char verbatim.
                    let start = self.i;
                    let len = match c {
                        0x00..=0x7F => 1,
                        0xC0..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        0xF0..=0xF7 => 4,
                        _ => return None,
                    };
                    if start + len > self.b.len() {
                        return None;
                    }
                    out.push_str(std::str::from_utf8(&self.b[start..start + len]).ok()?);
                    self.i += len;
                }
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let c = self.peek()?;
            self.i += 1;
            let d = (c as char).to_digit(16)?;
            v = v * 16 + d;
        }
        Some(v)
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        if self.eat(b'.') {
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        s.parse::<f64>().ok().map(Json::Num)
    }
}

/// Parse a JSON document into a Store. Tolerates missing optional fields
/// (free/docked/color/font_size get defaults) and skips notes lacking an id
/// or valid geometry. Field order does not matter. Returns None on any
/// syntactic failure or if the top level is not an object.
pub(crate) fn parse(s: &str) -> Option<Store> {
    let mut p = Parser::new(s);
    let root = p.value()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return None; // trailing garbage
    }
    if !matches!(root, Json::Obj(_)) {
        return None;
    }

    let version = root
        .get("version")
        .and_then(|v| v.int())
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1);
    let next_id = root
        .get("next_id")
        .and_then(|v| v.int())
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(1);

    let mut notes = Vec::new();
    if let Some(Json::Arr(items)) = root.get("notes") {
        for item in items {
            if let Some(n) = note_from(item) {
                notes.push(n);
            }
        }
    }
    Some(Store {
        version,
        next_id,
        notes,
    })
}

fn note_from(v: &Json) -> Option<NoteData> {
    // Required: id and valid geometry. Everything else defaults.
    let id = v.get("id")?.int().and_then(|i| u64::try_from(i).ok())?;
    let x = v.get("x")?.int()? as i32;
    let y = v.get("y")?.int()? as i32;
    let w = v.get("w")?.int()? as i32;
    let h = v.get("h")?.int()? as i32;
    if w <= 0 || h <= 0 {
        return None;
    }
    let text = match v.get("text") {
        Some(Json::Str(s)) => s.clone(),
        _ => String::new(),
    };
    let free = match v.get("free") {
        Some(Json::Bool(b)) => *b,
        _ => true,
    };
    let docked = v
        .get("docked")
        .and_then(|d| d.int())
        .and_then(|d| i8::try_from(d).ok())
        .unwrap_or(0);
    let color = v
        .get("color")
        .and_then(|c| c.int())
        .and_then(|c| u8::try_from(c).ok())
        .unwrap_or(0);
    let font_size = v
        .get("font_size")
        .and_then(|f| f.num())
        .map(|f| f as f32)
        .unwrap_or(16.0);
    Some(NoteData {
        id,
        text,
        x,
        y,
        w,
        h,
        free,
        docked,
        color,
        font_size,
    })
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_store() -> Store {
        Store {
            version: 1,
            next_id: 7,
            notes: vec![
                NoteData {
                    id: 1,
                    text: "plain \"quotes\" and back\\slash".to_string(),
                    x: 10,
                    y: 20,
                    w: 340,
                    h: 260,
                    free: true,
                    docked: 0,
                    color: 0,
                    font_size: 16.0,
                },
                NoteData {
                    id: 3,
                    text: "line one\nline two\twith tab\rand cr\u{0001}ctl".to_string(),
                    x: -50,
                    y: 0,
                    w: 200,
                    h: 150,
                    free: false,
                    docked: -1,
                    color: 4,
                    font_size: 13.5,
                },
                NoteData {
                    id: 6,
                    text: "café … 你好 🙂".to_string(),
                    x: 999,
                    y: 888,
                    w: 150,
                    h: 120,
                    free: true,
                    docked: 1,
                    color: 255,
                    font_size: 22.25,
                },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let original = sample_store();
        let json = to_json(&original);
        let parsed = parse(&json).expect("round-trip parse failed");
        assert_eq!(parsed, original);
    }

    #[test]
    fn garbage_and_missing_file_default() {
        assert!(parse("").is_none());
        assert!(parse("not json at all").is_none());
        assert!(parse("{\"version\":1,").is_none());
        assert!(parse("[1,2,3]").is_none());
        // load() on a non-existent path returns Store::default().
        let bogus = std::env::temp_dir().join("liquidnotes_no_such_file_921407.json");
        assert_eq!(load_from(&bogus), Store::default());
    }

    #[test]
    fn missing_optional_fields_get_defaults() {
        let json = r#"{"version":1,"next_id":9,"notes":[
            {"h":260,"w":340,"y":20,"x":10,"text":"hi","id":2}
        ]}"#;
        let store = parse(json).expect("parse failed");
        assert_eq!(store.next_id, 9);
        assert_eq!(store.notes.len(), 1);
        let n = &store.notes[0];
        assert_eq!(n.id, 2);
        assert_eq!(n.text, "hi");
        assert_eq!((n.x, n.y, n.w, n.h), (10, 20, 340, 260));
        assert!(n.free);
        assert_eq!(n.docked, 0);
        assert_eq!(n.color, 0);
        assert_eq!(n.font_size, 16.0);
    }

    #[test]
    fn skips_invalid_notes() {
        let json = r#"{"version":1,"next_id":5,"notes":[
            {"text":"no id","x":1,"y":2,"w":10,"h":10},
            {"id":1,"text":"bad geom","x":1,"y":2,"w":0,"h":10},
            {"id":2,"text":"ok","x":1,"y":2,"w":10,"h":10}
        ]}"#;
        let store = parse(json).expect("parse failed");
        assert_eq!(store.notes.len(), 1);
        assert_eq!(store.notes[0].id, 2);
    }

    #[test]
    fn unicode_escapes_unescape() {
        // Verbatim multibyte UTF-8 plus a \t escape pass through unchanged.
        let json = r#"{"version":1,"next_id":2,"notes":[
            {"id":1,"text":"aAé你🙂\tb","x":0,"y":0,"w":10,"h":10}
        ]}"#;
        let store = parse(json).expect("parse failed");
        assert_eq!(store.notes[0].text, "aAé你🙂\tb");

        // \uXXXX escapes: BMP chars and a surrogate pair (U+1F642).
        let json2 = "{\"version\":1,\"next_id\":2,\"notes\":[{\"id\":1,\
            \"text\":\"a\\u0041 \\u00e9 \\u4f60 \\ud83d\\ude42\",\
            \"x\":0,\"y\":0,\"w\":10,\"h\":10}]}";
        let store2 = parse(json2).expect("parse2 failed");
        assert_eq!(store2.notes[0].text, "aA \u{e9} \u{4f60} \u{1f642}");

        // Lone/mismatched surrogates are rejected.
        assert!(parse("{\"version\":1,\"next_id\":1,\"notes\":[{\"id\":1,\
            \"text\":\"x\\ud83dx\",\"x\":0,\"y\":0,\"w\":1,\"h\":1}]}")
            .is_none());
    }

    #[test]
    fn save_atomic_round_trip_on_disk() {
        let path = std::env::temp_dir().join(format!(
            "liquidnotes_test_{}.json",
            std::process::id()
        ));
        let original = sample_store();
        save_atomic_to(&path, &original).expect("save failed");
        let loaded = load_from(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, original);
    }
}
