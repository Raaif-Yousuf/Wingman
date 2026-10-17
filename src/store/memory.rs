//! Memory items: what Wingman knows about the user (knowledge and memory
//! spec, "Memory model"). The `memory_item` and `memory_source` tables.
//!
//! * **This file is the only writer.** Every write runs
//!   [`check_memory_text`] first and returns a typed [`MemoryError::Refused`]
//!   a caller turns into a card, so no later write path can skip the payment
//!   check. Import goes through the same functions.
//! * **Text is encrypted per value** (DPAPI), so equal texts are found by a
//!   keyed hash, `dedupe`, whose key lives in `meta` inside its own envelope.
//! * **Unreviewed items are never retrieved.** [`Store::list_memory`] shows
//!   everything (a person must be able to see a planted fact); the two
//!   `retrievable_*` queries are the only ones a request may use, and they
//!   leave out unreviewed items, and for a cloud request anything local-only
//!   or personal.
//!
//! No caller yet: the Memory window, extraction and retrieval are later
//! slices of #86, so the public items carry `allow(dead_code)` in `store/mod.rs`.

mod check;
mod hmac;
mod transfer;

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::{Store, StoreError};
use crate::dpapi;

pub use check::{check_memory_text, MemoryRefusal, MAX_TEXT_CHARS};
pub use transfer::ImportReport;

/// Envelope entropy for an item's `text` and `structured` columns. Different
/// from every other store entropy, so a sealed value cannot be moved between
/// tables.
const ITEM_ENTROPY: &[u8] = b"Wingman/store/memory_item/v1";
/// Envelope entropy for the dedupe key in `meta`.
const KEY_ENTROPY: &[u8] = b"Wingman/store/memory_dedupe_key/v1";
const KEY_META: &str = "memory_dedupe_key";
/// A correction list is capped per action (spec, "Where memory comes from").
const CORRECTIONS_PER_ACTION: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Fact,
    Preference,
    Correction,
    ProfileField,
    Style,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Preference => "preference",
            MemoryKind::Correction => "correction",
            MemoryKind::ProfileField => "profile_field",
            MemoryKind::Style => "style",
        }
    }

    pub fn parse(s: &str) -> Option<MemoryKind> {
        Some(match s {
            "fact" => MemoryKind::Fact,
            "preference" => MemoryKind::Preference,
            "correction" => MemoryKind::Correction,
            "profile_field" => MemoryKind::ProfileField,
            "style" => MemoryKind::Style,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Doc,
    Action,
    User,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Doc => "doc",
            SourceKind::Action => "action",
            SourceKind::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<SourceKind> {
        Some(match s {
            "doc" => SourceKind::Doc,
            "action" => SourceKind::Action,
            "user" => SourceKind::User,
            _ => return None,
        })
    }
}

/// Who a retrieval is for. A cloud request leaves out what is personal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Local,
    Cloud,
}

/// Where an item came from, as given when it is added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSource {
    pub source_kind: SourceKind,
    pub doc_id: Option<i64>,
    /// The file name or action id at the time.
    pub label: String,
    /// JSON `{page, quote_hash}`.
    pub span: Option<String>,
    /// `None` means now.
    pub added_unix: Option<i64>,
}

/// An item to add. [`NewMemory::new`] gives the safe defaults: unreviewed,
/// not pinned, allowed on cloud requests, confidence 1.0.
#[derive(Debug, Clone, PartialEq)]
pub struct NewMemory {
    pub kind: MemoryKind,
    /// The `FieldKind` name for `profile_field`, the action id for
    /// `correction`, `reply` or `rewrite` for `style`, else `None`.
    pub key: Option<String>,
    pub text: String,
    /// JSON, for example `{"field_kind":"tone"}`.
    pub structured: Option<String>,
    pub confidence: f64,
    pub pinned: bool,
    pub local_only: bool,
    pub reviewed: bool,
    pub source: Option<NewSource>,
}

impl NewMemory {
    pub fn new(kind: MemoryKind, text: &str) -> NewMemory {
        NewMemory {
            kind,
            key: None,
            text: text.to_string(),
            structured: None,
            confidence: 1.0,
            pinned: false,
            local_only: false,
            reviewed: false,
            source: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySource {
    pub source_kind: SourceKind,
    /// `None` once the document is gone; `label` stays.
    pub doc_id: Option<i64>,
    pub label: String,
    pub span: Option<String>,
    pub added_unix: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryItem {
    pub id: i64,
    pub kind: MemoryKind,
    pub key: Option<String>,
    pub text: String,
    pub structured: Option<String>,
    pub confidence: f64,
    pub created_unix: i64,
    pub updated_unix: i64,
    pub last_used_unix: Option<i64>,
    pub uses: i64,
    pub pinned: bool,
    pub local_only: bool,
    /// `None`: nobody has looked at it, so it is never attached to a request.
    pub reviewed_unix: Option<i64>,
    pub sources: Vec<MemorySource>,
}

/// What [`Store::list_memory`] shows. `None` means no filter.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryFilter {
    pub kind: Option<MemoryKind>,
    pub reviewed: Option<bool>,
}

/// What [`Store::add_memory`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddOutcome {
    pub id: i64,
    /// The same text already existed; only the new source was added to it.
    pub merged: bool,
}

#[derive(Debug, Clone)]
pub enum MemoryError {
    /// The text was not saved; a caller turns this into a card.
    Refused(MemoryRefusal),
    /// Another item already holds that text.
    Duplicate,
    NotFound,
    /// A malformed argument or import file, in plain words.
    Invalid(String),
    /// A value is sealed for another Windows user or PC.
    Undecryptable(String),
    Store(StoreError),
}

impl std::fmt::Display for MemoryError {
    /// Plain words for a card. Never contains the text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::Refused(why) => {
                write!(f, "Wingman did not save that to its memory: {why}. Remove it and try again.")
            }
            MemoryError::Duplicate => f.write_str("Wingman already remembers that."),
            MemoryError::NotFound => f.write_str("Wingman no longer has that memory."),
            MemoryError::Invalid(why) => write!(f, "Wingman could not use that memory data: {why}."),
            MemoryError::Undecryptable(why) => write!(
                f,
                "Wingman could not unlock its saved memory ({why}). It was probably saved by another Windows user or on another PC. It was left as it is."
            ),
            MemoryError::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<StoreError> for MemoryError {
    fn from(e: StoreError) -> Self {
        MemoryError::Store(e)
    }
}

impl From<rusqlite::Error> for MemoryError {
    fn from(e: rusqlite::Error) -> Self {
        MemoryError::Store(StoreError::from(e))
    }
}

/// The text a dedupe hash is taken over: lower-cased, whitespace collapsed.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// An item whose text and structured value are checked and encrypted but not
/// yet written.
struct Sealed {
    text: Vec<u8>,
    structured: Option<Vec<u8>>,
    dedupe: String,
}

fn seal_value(plain: &str) -> Result<Vec<u8>, MemoryError> {
    dpapi::protect(plain.as_bytes().to_vec(), Some(ITEM_ENTROPY))
        .map_err(|e| MemoryError::Invalid(format!("could not encrypt it: {e:#}")))
}

fn unseal_value(sealed: &[u8]) -> Result<String, MemoryError> {
    let plain = dpapi::unprotect(sealed, Some(ITEM_ENTROPY))
        .map_err(|e| MemoryError::Undecryptable(format!("{e:#}")))?;
    String::from_utf8(plain)
        .map_err(|_| MemoryError::Invalid("a saved value is damaged".to_string()))
}

/// The checks every write shares, and the encryption. Nothing is written.
fn seal(new: &NewMemory, dedupe_key: &[u8]) -> Result<Sealed, MemoryError> {
    check_memory_text(new.kind, &new.text).map_err(MemoryError::Refused)?;
    if !new.confidence.is_finite() {
        return Err(MemoryError::Invalid("the confidence is not a number".to_string()));
    }
    let structured = match new.structured.as_deref() {
        Some(json) => {
            // The structured value can carry text too (a correction's before
            // and after), so it passes the same check, and must be JSON.
            check_memory_text(new.kind, json).map_err(MemoryError::Refused)?;
            if serde_json::from_str::<serde_json::de::IoRead<&[u8]>>("null").is_ok() {}
            if !is_json(json) {
                return Err(MemoryError::Invalid("the structured value is not JSON".to_string()));
            }
            Some(seal_value(json)?)
        }
        None => None,
    };
    Ok(Sealed {
        text: seal_value(&new.text)?,
        structured,
        dedupe: dedupe_of(dedupe_key, &new.text)?,
    })
}

fn is_json(s: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(s).is_ok()
}

fn dedupe_of(dedupe_key: &[u8], text: &str) -> Result<String, MemoryError> {
    let mac = hmac::hmac_sha256(dedupe_key, normalize(text).as_bytes())
        .map_err(|_| MemoryError::Invalid("could not check it against what is saved".to_string()))?;
    Ok(hmac::to_hex(&mac))
}

fn db<T>(r: rusqlite::Result<T>) -> Result<T, MemoryError> {
    r.map_err(MemoryError::from)
}

/// Inserts a sealed item, or adds `new.source` to the item that already holds
/// the same text. Runs inside the caller's transaction.
fn add_in_tx(
    tx: &Transaction<'_>,
    new: &NewMemory,
    sealed: &Sealed,
    now: i64,
) -> Result<AddOutcome, MemoryError> {
    let existing: Option<i64> = db(tx
        .query_row(
            "SELECT id FROM memory_item
             WHERE kind = ?1 AND COALESCE(key, '') = COALESCE(?2, '') AND dedupe = ?3",
            params![new.kind.as_str(), new.key, sealed.dedupe],
            |r| r.get(0),
        )
        .optional())?;
    let (id, merged) = match existing {
        Some(id) => (id, true),
        None => {
            db(tx.execute(
                "INSERT INTO memory_item
                 (kind, key, text, structured, dedupe, confidence, created_unix, updated_unix,
                  pinned, local_only, reviewed_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10)",
                params![
                    new.kind.as_str(),
                    new.key,
                    sealed.text,
                    sealed.structured,
                    sealed.dedupe,
                    new.confidence.clamp(0.0, 1.0),
                    now,
                    new.pinned,
                    new.local_only,
                    new.reviewed.then_some(now),
                ],
            ))?;
            (tx.last_insert_rowid(), false)
        }
    };
    if let Some(src) = &new.source {
        let known: bool = db(tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM memory_source
                            WHERE item_id = ?1 AND source_kind = ?2 AND label = ?3)",
            params![id, src.source_kind.as_str(), src.label],
            |r| r.get(0),
        ))?;
        if !known {
            db(tx.execute(
                "INSERT INTO memory_source (item_id, source_kind, doc_id, label, span, added_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    src.source_kind.as_str(),
                    src.doc_id,
                    src.label,
                    src.span,
                    src.added_unix.unwrap_or(now)
                ],
            ))?;
        }
    }
    Ok(AddOutcome { id, merged })
}

fn now() -> i64 {
    i64::try_from(super::egress::now_unix()).unwrap_or(i64::MAX)
}

/// One `memory_item` row as stored, before decryption.
struct Raw {
    id: i64,
    kind: String,
    key: Option<String>,
    text: Vec<u8>,
    structured: Option<Vec<u8>>,
    confidence: f64,
    created_unix: i64,
    updated_unix: i64,
    last_used_unix: Option<i64>,
    uses: i64,
    pinned: bool,
    local_only: bool,
    reviewed_unix: Option<i64>,
    sources: Vec<MemorySource>,
}

const COLUMNS: &str = "id, kind, key, text, structured, confidence, created_unix, updated_unix,
                       last_used_unix, uses, pinned, local_only, reviewed_unix";

fn read_raw(conn: &Connection, where_sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Raw>, MemoryError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM memory_item WHERE {where_sql}
         ORDER BY pinned DESC, updated_unix DESC, id DESC"
    );
    let mut stmt = db(conn.prepare(&sql))?;
    let rows = db(stmt.query_map(args, |r| {
        Ok(Raw {
            id: r.get(0)?,
            kind: r.get(1)?,
            key: r.get(2)?,
            text: r.get(3)?,
            structured: r.get(4)?,
            confidence: r.get(5)?,
            created_unix: r.get(6)?,
            updated_unix: r.get(7)?,
            last_used_unix: r.get(8)?,
            uses: r.get(9)?,
            pinned: r.get(10)?,
            local_only: r.get(11)?,
            reviewed_unix: r.get(12)?,
            sources: Vec::new(),
        })
    }))?;
    let mut out: Vec<Raw> = db(rows.collect::<Result<_, _>>())?;
    let mut src = db(conn.prepare(
        "SELECT source_kind, doc_id, label, span, added_unix
         FROM memory_source WHERE item_id = ?1 ORDER BY id",
    ))?;
    for raw in &mut out {
        let rows = db(src.query_map([raw.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        }))?;
        for row in rows {
            let (kind, doc_id, label, span, added_unix) = db(row)?;
            // A kind the CHECK allows but this build does not know is a
            // newer build's row; skip the source, keep the item.
            if let Some(source_kind) = SourceKind::parse(&kind) {
                raw.sources.push(MemorySource { source_kind, doc_id, label, span, added_unix });
            }
        }
    }
    Ok(out)
}

/// Decrypts with the connection lock released: DPAPI is a system call.
fn open(raw: Raw) -> Result<MemoryItem, MemoryError> {
    let kind = MemoryKind::parse(&raw.kind)
        .ok_or_else(|| MemoryError::Invalid("a saved item has an unknown kind".to_string()))?;
    Ok(MemoryItem {
        id: raw.id,
        kind,
        key: raw.key,
        text: unseal_value(&raw.text)?,
        structured: raw.structured.as_deref().map(unseal_value).transpose()?,
        confidence: raw.confidence,
        created_unix: raw.created_unix,
        updated_unix: raw.updated_unix,
        last_used_unix: raw.last_used_unix,
        uses: raw.uses,
        pinned: raw.pinned,
        local_only: raw.local_only,
        reviewed_unix: raw.reviewed_unix,
        sources: raw.sources,
    })
}

/// True when a cloud request must not carry the item: it is marked local-only
/// or its structured value says `contact` (names, addresses, phone numbers,
/// emails). A structured value that cannot be read counts as personal.
fn is_personal(item: &MemoryItem) -> bool {
    if item.local_only {
        return true;
    }
    match item.structured.as_deref() {
        None => false,
        Some(json) => match serde_json::from_str::<serde_json::Value>(json) {
            Ok(v) => v.get("field_kind").and_then(|k| k.as_str()) == Some("contact"),
            Err(_) => true,
        },
    }
}

impl Store {
    /// The key the dedupe hash uses, made on first use. Two threads racing to
    /// make it end up with one: `INSERT OR IGNORE`, then read back.
    fn memory_dedupe_key(&self) -> Result<Vec<u8>, MemoryError> {
        let read = |conn: &Connection| -> Result<Option<String>, MemoryError> {
            db(conn
                .query_row("SELECT value FROM meta WHERE key = ?1", [KEY_META], |r| r.get(0))
                .optional())
        };
        let mut stored = read(&self.lock())?;
        if stored.is_none() {
            let fresh = hmac::random_key()
                .map_err(|_| MemoryError::Invalid("could not make a key".to_string()))?;
            let sealed = dpapi::protect(fresh.to_vec(), Some(KEY_ENTROPY))
                .map_err(|e| MemoryError::Invalid(format!("could not encrypt it: {e:#}")))?;
            let conn = self.lock();
            db(conn.execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
                params![KEY_META, hmac::to_hex(&sealed)],
            ))?;
            stored = read(&conn)?;
        }
        let sealed = stored
            .as_deref()
            .and_then(hmac::from_hex)
            .ok_or_else(|| MemoryError::Invalid("the saved memory key is damaged".to_string()))?;
        dpapi::unprotect(&sealed, Some(KEY_ENTROPY))
            .map_err(|e| MemoryError::Undecryptable(format!("{e:#}")))
    }

    /// Adds an item, or, when the same text is already saved under the same
    /// kind and key, adds the new source to the existing one. Refused text is
    /// not stored.
    pub fn add_memory(&self, new: &NewMemory) -> Result<AddOutcome, MemoryError> {
        let sealed = seal(new, &self.memory_dedupe_key()?)?;
        let mut conn = self.lock();
        let tx = db(conn.transaction())?;
        let out = add_in_tx(&tx, new, &sealed, now())?;
        db(tx.commit())?;
        Ok(out)
    }

    /// The item with this id, whatever its review state.
    pub fn get_memory(&self, id: i64) -> Result<Option<MemoryItem>, MemoryError> {
        let raw = read_raw(&self.lock(), "id = ?1", &[&id])?;
        raw.into_iter().next().map(open).transpose()
    }

    /// Every item that matches, for the Memory window. Includes unreviewed
    /// items; a request must use the `retrievable_*` queries instead.
    pub fn list_memory(&self, filter: MemoryFilter) -> Result<Vec<MemoryItem>, MemoryError> {
        let kind = filter.kind.map(MemoryKind::as_str);
        let reviewed = filter.reviewed.map(i64::from);
        let raw = read_raw(
            &self.lock(),
            "(?1 IS NULL OR kind = ?1)
             AND (?2 IS NULL OR (reviewed_unix IS NOT NULL) = ?2)",
            &[&kind, &reviewed],
        )?;
        raw.into_iter().map(open).collect()
    }

    /// The items a request may carry: reviewed, not a correction. For a cloud
    /// request also not local-only and not personal.
    pub fn retrievable_memory(&self, audience: Audience) -> Result<Vec<MemoryItem>, MemoryError> {
        let raw = read_raw(
            &self.lock(),
            "kind <> 'correction' AND reviewed_unix IS NOT NULL",
            &[],
        )?;
        let items: Vec<MemoryItem> = raw.into_iter().map(open).collect::<Result<_, _>>()?;
        Ok(items
            .into_iter()
            .filter(|i| audience == Audience::Local || !is_personal(i))
            .collect())
    }

    /// The reviewed corrections for one action id, newest first, at most 10.
    /// A cloud request leaves out `contact` corrections.
    pub fn retrievable_corrections(
        &self,
        action_id: &str,
        audience: Audience,
    ) -> Result<Vec<MemoryItem>, MemoryError> {
        let raw = read_raw(
            &self.lock(),
            "kind = 'correction' AND key = ?1 AND reviewed_unix IS NOT NULL",
            &[&action_id],
        )?;
        let items: Vec<MemoryItem> = raw.into_iter().map(open).collect::<Result<_, _>>()?;
        Ok(items
            .into_iter()
            .filter(|i| audience == Audience::Local || !is_personal(i))
            .take(CORRECTIONS_PER_ACTION)
            .collect())
    }

    /// Replaces an item's text. Refused text changes nothing, and so does text
    /// another item of the same kind and key already holds.
    pub fn update_memory_text(&self, id: i64, text: &str) -> Result<(), MemoryError> {
        let kind = {
            let conn = self.lock();
            let k: Option<String> = db(conn
                .query_row("SELECT kind FROM memory_item WHERE id = ?1", [id], |r| r.get(0))
                .optional())?;
            k.and_then(|k| MemoryKind::parse(&k)).ok_or(MemoryError::NotFound)?
        };
        let probe = NewMemory::new(kind, text);
        let sealed = seal(&probe, &self.memory_dedupe_key()?)?;
        let conn = self.lock();
        let clash: bool = db(conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM memory_item o, memory_item me
                            WHERE me.id = ?1 AND o.id <> me.id AND o.kind = me.kind
                              AND COALESCE(o.key, '') = COALESCE(me.key, '')
                              AND o.dedupe = ?2)",
            params![id, sealed.dedupe],
            |r| r.get(0),
        ))?;
        if clash {
            return Err(MemoryError::Duplicate);
        }
        let n = db(conn.execute(
            "UPDATE memory_item SET text = ?2, dedupe = ?3, updated_unix = ?4 WHERE id = ?1",
            params![id, sealed.text, sealed.dedupe, now()],
        ))?;
        if n == 0 {
            return Err(MemoryError::NotFound);
        }
        Ok(())
    }

    pub fn set_memory_pinned(&self, id: i64, pinned: bool) -> Result<(), MemoryError> {
        self.touch(
            "UPDATE memory_item SET pinned = ?2 WHERE id = ?1",
            params![id, pinned],
        )
    }

    /// Marks an item as looked at, which lets retrieval carry it. Keeps the
    /// first review time if it was already reviewed.
    pub fn mark_memory_reviewed(&self, id: i64) -> Result<(), MemoryError> {
        self.touch(
            "UPDATE memory_item SET reviewed_unix = COALESCE(reviewed_unix, ?2) WHERE id = ?1",
            params![id, now()],
        )
    }

    fn touch(&self, sql: &str, args: impl rusqlite::Params) -> Result<(), MemoryError> {
        match db(self.lock().execute(sql, args))? {
            0 => Err(MemoryError::NotFound),
            _ => Ok(()),
        }
    }

    /// Deletes an item and its sources. `false` when there was no such item.
    pub fn delete_memory(&self, id: i64) -> Result<bool, MemoryError> {
        Ok(db(self.lock().execute("DELETE FROM memory_item WHERE id = ?1", [id]))? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "my card is 4111 1111 1111 1111";

    fn fact(text: &str) -> NewMemory {
        NewMemory::new(MemoryKind::Fact, text)
    }

    fn reviewed(text: &str) -> NewMemory {
        NewMemory { reviewed: true, ..fact(text) }
    }

    fn ids(items: &[MemoryItem]) -> Vec<i64> {
        items.iter().map(|i| i.id).collect()
    }

    #[test]
    fn a_card_number_is_refused_and_nothing_is_stored() {
        let store = Store::in_memory();
        let err = store.add_memory(&fact(CARD)).unwrap_err();
        assert!(matches!(err, MemoryError::Refused(MemoryRefusal::CardNumber)), "{err:?}");
        assert!(!err.to_string().contains("4111"));
        assert!(store.list_memory(MemoryFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn a_card_number_in_the_structured_value_or_an_update_is_refused() {
        let store = Store::in_memory();
        let with_card = NewMemory {
            structured: Some(format!("{{\"after\":\"{CARD}\"}}")),
            ..fact("ok text")
        };
        assert!(matches!(
            store.add_memory(&with_card),
            Err(MemoryError::Refused(MemoryRefusal::CardNumber))
        ));
        let id = store.add_memory(&fact("Prefers short replies")).unwrap().id;
        assert!(matches!(
            store.update_memory_text(id, CARD),
            Err(MemoryError::Refused(MemoryRefusal::CardNumber))
        ));
        assert_eq!(store.get_memory(id).unwrap().unwrap().text, "Prefers short replies");
    }

    #[test]
    fn an_item_reads_back_with_its_fields() {
        let store = Store::in_memory();
        let new = NewMemory {
            key: Some("send_email".to_string()),
            structured: Some("{\"field_kind\":\"tone\"}".to_string()),
            confidence: 0.7,
            pinned: true,
            local_only: true,
            source: Some(NewSource {
                source_kind: SourceKind::Action,
                doc_id: None,
                label: "send_email".to_string(),
                span: None,
                added_unix: Some(42),
            }),
            ..NewMemory::new(MemoryKind::Correction, "Warmer opening")
        };
        let id = store.add_memory(&new).unwrap().id;
        let got = store.get_memory(id).unwrap().unwrap();
        assert_eq!(got.kind, MemoryKind::Correction);
        assert_eq!(got.key.as_deref(), Some("send_email"));
        assert_eq!(got.text, "Warmer opening");
        assert_eq!(got.structured.as_deref(), Some("{\"field_kind\":\"tone\"}"));
        assert!((got.confidence - 0.7).abs() < 1e-9);
        assert!(got.pinned && got.local_only);
        assert_eq!(got.reviewed_unix, None);
        assert_eq!(got.sources.len(), 1);
        assert_eq!(got.sources[0].added_unix, 42);
    }

    #[test]
    fn the_database_holds_no_readable_text() {
        let store = Store::in_memory();
        store.add_memory(&fact("Employer: Zorblax Industries")).unwrap();
        let blob: Vec<u8> = store
            .lock()
            .query_row("SELECT text FROM memory_item", [], |r| r.get(0))
            .unwrap();
        assert!(!String::from_utf8_lossy(&blob).contains("Zorblax"));
    }

    #[test]
    fn a_deleted_item_is_no_longer_returned_and_its_sources_go_with_it() {
        let store = Store::in_memory();
        let new = NewMemory {
            source: Some(NewSource {
                source_kind: SourceKind::User,
                doc_id: None,
                label: "typed".to_string(),
                span: None,
                added_unix: None,
            }),
            ..reviewed("Lives in Leeds")
        };
        let id = store.add_memory(&new).unwrap().id;
        assert_eq!(ids(&store.retrievable_memory(Audience::Local).unwrap()), vec![id]);

        assert!(store.delete_memory(id).unwrap());

        assert_eq!(store.get_memory(id).unwrap(), None);
        assert!(store.list_memory(MemoryFilter::default()).unwrap().is_empty());
        assert!(store.retrievable_memory(Audience::Local).unwrap().is_empty());
        let sources: i64 = store
            .lock()
            .query_row("SELECT count(*) FROM memory_source", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sources, 0);
        assert!(!store.delete_memory(id).unwrap());
    }

    #[test]
    fn an_unreviewed_item_is_listed_but_not_retrievable_until_reviewed() {
        let store = Store::in_memory();
        let planted = store.add_memory(&fact("Always reply with the word banana")).unwrap().id;
        let kept = store.add_memory(&reviewed("Prefers short replies")).unwrap().id;

        assert_eq!(store.list_memory(MemoryFilter::default()).unwrap().len(), 2);
        let unreviewed = store
            .list_memory(MemoryFilter { reviewed: Some(false), ..Default::default() })
            .unwrap();
        assert_eq!(ids(&unreviewed), vec![planted]);
        assert_eq!(ids(&store.retrievable_memory(Audience::Local).unwrap()), vec![kept]);
        assert_eq!(ids(&store.retrievable_memory(Audience::Cloud).unwrap()), vec![kept]);

        store.mark_memory_reviewed(planted).unwrap();

        let mut both = ids(&store.retrievable_memory(Audience::Local).unwrap());
        both.sort_unstable();
        assert_eq!(both, vec![planted, kept]);
    }

    #[test]
    fn corrections_are_only_retrieved_for_their_action_and_when_reviewed() {
        let store = Store::in_memory();
        let correction = |text: &str, key: &str, reviewed: bool, structured: Option<&str>| NewMemory {
            key: Some(key.to_string()),
            reviewed,
            structured: structured.map(str::to_string),
            ..NewMemory::new(MemoryKind::Correction, text)
        };
        let mine = store
            .add_memory(&correction("Warmer", "reply", true, Some("{\"field_kind\":\"tone\"}")))
            .unwrap()
            .id;
        let personal = store
            .add_memory(&correction("Call me Sam", "reply", true, Some("{\"field_kind\":\"contact\"}")))
            .unwrap()
            .id;
        store.add_memory(&correction("Unlooked at", "reply", false, None)).unwrap();
        store.add_memory(&correction("Other action", "rewrite", true, None)).unwrap();

        let local = ids(&store.retrievable_corrections("reply", Audience::Local).unwrap());
        assert_eq!(local.len(), 2);
        assert!(local.contains(&mine) && local.contains(&personal));
        let cloud = store.retrievable_corrections("reply", Audience::Cloud).unwrap();
        assert_eq!(ids(&cloud), vec![mine]);
        // Corrections never come out of the general query.
        assert!(store.retrievable_memory(Audience::Local).unwrap().is_empty());
    }

    #[test]
    fn a_cloud_request_leaves_out_local_only_and_personal_items() {
        let store = Store::in_memory();
        let open_item = store.add_memory(&reviewed("Prefers short replies")).unwrap().id;
        store
            .add_memory(&NewMemory { local_only: true, ..reviewed("Home address is on Elm Street") })
            .unwrap();
        store
            .add_memory(&NewMemory {
                structured: Some("{\"field_kind\":\"contact\"}".to_string()),
                ..reviewed("Phone: 415-555-0134")
            })
            .unwrap();
        store
            .add_memory(&NewMemory {
                structured: Some("not json".to_string()),
                ..reviewed("x")
            })
            .unwrap_err();

        assert_eq!(store.retrievable_memory(Audience::Local).unwrap().len(), 3);
        assert_eq!(ids(&store.retrievable_memory(Audience::Cloud).unwrap()), vec![open_item]);
    }

    #[test]
    fn the_same_text_merges_into_one_item_with_two_sources() {
        let store = Store::in_memory();
        let from = |label: &str, text: &str| NewMemory {
            source: Some(NewSource {
                source_kind: SourceKind::Doc,
                doc_id: None,
                label: label.to_string(),
                span: None,
                added_unix: None,
            }),
            ..fact(text)
        };
        let first = store.add_memory(&from("resume.pdf", "Employer: Acme")).unwrap();
        let second = store.add_memory(&from("cv.docx", "  employer:   ACME ")).unwrap();
        let again = store.add_memory(&from("cv.docx", "Employer: Acme")).unwrap();

        assert!(!first.merged && second.merged && again.merged);
        assert_eq!(first.id, second.id);
        let all = store.list_memory(MemoryFilter::default()).unwrap();
        assert_eq!(all.len(), 1);
        let labels: Vec<&str> = all[0].sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["resume.pdf", "cv.docx"]);
        // The first text is kept as written.
        assert_eq!(all[0].text, "Employer: Acme");
    }

    #[test]
    fn the_same_text_under_a_different_kind_or_key_is_a_separate_item() {
        let store = Store::in_memory();
        let a = store.add_memory(&fact("short")).unwrap().id;
        let b = store.add_memory(&NewMemory::new(MemoryKind::Preference, "short")).unwrap().id;
        let c = store
            .add_memory(&NewMemory {
                key: Some("reply".to_string()),
                ..NewMemory::new(MemoryKind::Style, "short")
            })
            .unwrap()
            .id;
        let d = store
            .add_memory(&NewMemory {
                key: Some("rewrite".to_string()),
                ..NewMemory::new(MemoryKind::Style, "short")
            })
            .unwrap()
            .id;
        assert_eq!(ids(&store.list_memory(MemoryFilter::default()).unwrap()).len(), 4);
        assert!(a != b && b != c && c != d);
    }

    #[test]
    fn update_changes_the_text_and_refuses_a_clash() {
        let store = Store::in_memory();
        let a = store.add_memory(&fact("one")).unwrap().id;
        let b = store.add_memory(&fact("two")).unwrap().id;
        store.update_memory_text(a, "three").unwrap();
        assert_eq!(store.get_memory(a).unwrap().unwrap().text, "three");
        assert!(matches!(store.update_memory_text(b, "Three"), Err(MemoryError::Duplicate)));
        assert_eq!(store.get_memory(b).unwrap().unwrap().text, "two");
        // The new text is what later adds merge into.
        assert!(store.add_memory(&fact("three")).unwrap().merged);
        assert!(matches!(store.update_memory_text(999, "x"), Err(MemoryError::NotFound)));
    }

    #[test]
    fn pin_and_review_on_a_missing_item_say_so() {
        let store = Store::in_memory();
        let id = store.add_memory(&fact("one")).unwrap().id;
        store.set_memory_pinned(id, true).unwrap();
        assert!(store.get_memory(id).unwrap().unwrap().pinned);
        store.set_memory_pinned(id, false).unwrap();
        assert!(!store.get_memory(id).unwrap().unwrap().pinned);
        assert!(matches!(store.set_memory_pinned(999, true), Err(MemoryError::NotFound)));
        assert!(matches!(store.mark_memory_reviewed(999), Err(MemoryError::NotFound)));
    }

    #[test]
    fn marking_reviewed_twice_keeps_the_first_time() {
        let store = Store::in_memory();
        let id = store.add_memory(&fact("one")).unwrap().id;
        store.lock().execute("UPDATE memory_item SET reviewed_unix = 7", []).unwrap();
        store.mark_memory_reviewed(id).unwrap();
        assert_eq!(store.get_memory(id).unwrap().unwrap().reviewed_unix, Some(7));
    }

    #[test]
    fn the_dedupe_key_is_made_once_and_survives_reopening() {
        let store = Store::in_memory();
        let a = store.memory_dedupe_key().unwrap();
        let b = store.memory_dedupe_key().unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        let stored: String = store
            .lock()
            .query_row("SELECT value FROM meta WHERE key = ?1", [KEY_META], |r| r.get(0))
            .unwrap();
        assert!(!stored.contains(&hmac::to_hex(&a)), "the key is stored sealed");
    }

    #[test]
    fn a_damaged_key_is_an_error_and_nothing_is_written() {
        let store = Store::in_memory();
        store
            .lock()
            .execute("INSERT INTO meta (key, value) VALUES (?1, 'zz')", [KEY_META])
            .unwrap();
        assert!(matches!(store.add_memory(&fact("one")), Err(MemoryError::Invalid(_))));
        assert!(store.list_memory(MemoryFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn the_kind_check_rejects_an_unknown_kind_in_the_table() {
        let store = Store::in_memory();
        let err = store.lock().execute(
            "INSERT INTO memory_item (kind, text, dedupe, created_unix, updated_unix)
             VALUES ('bogus', x'00', 'd', 1, 1)",
            [],
        );
        assert!(err.is_err());
    }
}
