//! Lenient parsing of the JSON column of the dump files.
//!
//! Open Library records are user-edited and inconsistent: fields like
//! `description` and `bio` may be a plain string or a `{"type": …, "value": …}`
//! object, arrays may contain junk entries, and numbers sometimes arrive as
//! strings. Every custom deserializer here accepts any JSON value and extracts
//! what it can, so a malformed field never rejects the whole record.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Trims key prefixes like `/authors/OL23919A` down to `OL23919A`.
pub fn short_key(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// URL slugs stay comfortably below btree index entry limits.
const SLUG_MAX_CHARS: usize = 100;

/// Transliterate to ASCII (Достоевский → dostoevskii), lowercase, collapse
/// runs of anything non-alphanumeric to single hyphens, and clamp.
///
/// Returns an empty string when nothing survives (punctuation-only input) —
/// callers fall back to the record's open_library_id. The output never
/// contains `--`, which lets slug deduplication append `--N` suffixes that
/// provably can't collide with any natural slug.
pub fn slugify(input: &str) -> String {
    let ascii = deunicode::deunicode(input).to_lowercase();
    let mut slug = String::with_capacity(ascii.len().min(SLUG_MAX_CHARS));
    for ch in ascii.chars() {
        if slug.len() >= SLUG_MAX_CHARS {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// First plausible 4-digit year found in a free-form publish date
/// ("March 2005", "1995-03-01", "19??", …).
pub fn extract_year(date: &str) -> Option<i32> {
    let bytes = date.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start == 4 {
                let year: i32 = date[start..i].parse().ok()?;
                if (1000..=2100).contains(&year) {
                    return Some(year);
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

#[derive(Default, Deserialize)]
pub struct Author {
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub personal_name: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub birth_date: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub death_date: Option<String>,
    #[serde(default, deserialize_with = "text_or_value")]
    pub bio: Option<String>,
    #[serde(default, deserialize_with = "int_array")]
    pub photos: Vec<i32>,
}

#[derive(Default, Deserialize)]
pub struct Work {
    #[serde(default, deserialize_with = "lenient_string")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub subtitle: Option<String>,
    #[serde(default, deserialize_with = "text_or_value")]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub first_publish_date: Option<String>,
    #[serde(default, deserialize_with = "int_array")]
    pub covers: Vec<i32>,
    #[serde(default, deserialize_with = "string_array")]
    pub subjects: Vec<String>,
    /// Work authors are `[{"author": {"key": …}}]`, sometimes `[{"author": "/authors/…"}]`.
    #[serde(default, deserialize_with = "author_role_keys", rename = "authors")]
    pub author_keys: Vec<String>,
}

#[derive(Default, Deserialize)]
pub struct Edition {
    #[serde(default, deserialize_with = "lenient_string")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub subtitle: Option<String>,
    /// Nearly always a single work.
    #[serde(default, deserialize_with = "key_ref_array", rename = "works")]
    pub work_keys: Vec<String>,
    #[serde(default, deserialize_with = "string_array")]
    pub publishers: Vec<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub publish_date: Option<String>,
    #[serde(default, deserialize_with = "lenient_int")]
    pub number_of_pages: Option<i32>,
    #[serde(default, deserialize_with = "string_array")]
    pub isbn_10: Vec<String>,
    #[serde(default, deserialize_with = "string_array")]
    pub isbn_13: Vec<String>,
    #[serde(default, deserialize_with = "int_array")]
    pub covers: Vec<i32>,
    #[serde(default, deserialize_with = "key_ref_array")]
    pub languages: Vec<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub physical_format: Option<String>,
    #[serde(default, deserialize_with = "string_array")]
    pub series: Vec<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub edition_name: Option<String>,
    /// Internet Archive id.
    #[serde(default, deserialize_with = "lenient_string")]
    pub ocaid: Option<String>,
    #[serde(default, deserialize_with = "identifiers")]
    pub identifiers: Identifiers,
}

impl Edition {
    pub fn publish_year(&self) -> Option<i32> {
        self.publish_date.as_deref().and_then(extract_year)
    }
}

/// External ids from the edition's `identifiers` object; each value is an
/// array in the dump, of which we keep the first entry.
#[derive(Default)]
pub struct Identifiers {
    pub amazon: Option<String>,
    pub goodreads: Option<String>,
    pub google: Option<String>,
}

fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A string, tolerating numbers and ignoring anything else.
fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(value_to_string(&Value::deserialize(d)?))
}

/// A string or an Open Library text object `{"type": "/type/text", "value": …}`.
fn text_or_value<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match &v {
        Value::Object(map) => map.get("value").and_then(value_to_string),
        _ => value_to_string(&v),
    })
}

/// An integer, tolerating numeric strings.
fn lenient_int<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i32>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

fn lenient_array<T>(v: Value, f: impl Fn(&Value) -> Option<T>) -> Vec<T> {
    match v {
        Value::Array(items) => items.iter().filter_map(f).collect(),
        _ => Vec::new(),
    }
}

fn string_array<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(lenient_array(Value::deserialize(d)?, value_to_string))
}

/// Cover/photo ids; -1 marks a deleted image, so keep positive ids only.
fn int_array<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<i32>, D::Error> {
    Ok(lenient_array(Value::deserialize(d)?, |v| {
        v.as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .filter(|&n| n > 0)
    }))
}

fn ref_key(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(short_key(s).to_string()),
        Value::Object(map) => map.get("key").and_then(ref_key),
        _ => None,
    }
}

/// An array of `{"key": "/works/OL1W"}` refs (or bare key strings).
fn key_ref_array<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(lenient_array(Value::deserialize(d)?, ref_key))
}

fn identifiers<'de, D: Deserializer<'de>>(d: D) -> Result<Identifiers, D::Error> {
    let v = Value::deserialize(d)?;
    let first = |key: &str| match v.get(key) {
        Some(Value::Array(items)) => items.iter().find_map(value_to_string),
        Some(other) => value_to_string(other),
        None => None,
    };
    Ok(Identifiers {
        amazon: first("amazon"),
        goodreads: first("goodreads"),
        google: first("google"),
    })
}

/// Work-style author list: `[{"author": {"key": …}, "type": …}]`.
fn author_role_keys<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(lenient_array(Value::deserialize(d)?, |v| match v {
        Value::Object(map) => map.get("author").and_then(ref_key).or_else(|| ref_key(v)),
        _ => ref_key(v),
    }))
}
