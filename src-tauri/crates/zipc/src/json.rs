//! A minimal JSON writer for `--json` output.
//!
//! The output is a handful of flat objects with strings, integers, booleans
//! and nulls, so a dependency-free builder is enough; the only subtle part,
//! string escaping (RFC 8259 section 7), is here and unit-tested.

use std::fmt::Write;

/// Append `s` to `out` as a quoted JSON string.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            // Valid in JSON, but not in JavaScript string literals before ES2019.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A JSON value for [`Object::field`].
pub enum Value<'a> {
    Str(&'a str),
    U64(u64),
    F64(f64),
    Bool(bool),
    Null,
    /// Already-serialized JSON (a nested object or array).
    Raw(&'a str),
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(s: &'a str) -> Self {
        Value::Str(s)
    }
}
impl From<u64> for Value<'_> {
    fn from(n: u64) -> Self {
        Value::U64(n)
    }
}
impl From<bool> for Value<'_> {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl<'a, T: Into<Value<'a>>> From<Option<T>> for Value<'a> {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

/// Builds one JSON object, fields in insertion order.
pub struct Object {
    buf: String,
}

impl Object {
    pub fn new() -> Self {
        Object {
            buf: String::from("{"),
        }
    }

    pub fn field<'a>(mut self, key: &str, value: impl Into<Value<'a>>) -> Self {
        if self.buf.len() > 1 {
            self.buf.push(',');
        }
        write_str(&mut self.buf, key);
        self.buf.push(':');
        match value.into() {
            Value::Str(s) => write_str(&mut self.buf, s),
            Value::U64(n) => {
                let _ = write!(self.buf, "{n}");
            }
            // JSON has no NaN or infinity.
            Value::F64(x) if x.is_finite() => {
                let _ = write!(self.buf, "{x}");
            }
            Value::F64(_) | Value::Null => self.buf.push_str("null"),
            Value::Bool(b) => self.buf.push_str(if b { "true" } else { "false" }),
            Value::Raw(r) => self.buf.push_str(r),
        }
        self
    }

    pub fn finish(mut self) -> String {
        self.buf.push('}');
        self.buf
    }
}

/// A JSON array of already-serialized values.
pub fn array<I: IntoIterator<Item = String>>(items: I) -> String {
    let mut out = String::from("[");
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&item);
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(s: &str) -> String {
        let mut out = String::new();
        write_str(&mut out, s);
        out
    }

    #[test]
    fn escapes() {
        assert_eq!(quoted("plain/é.txt"), r#""plain/é.txt""#);
        assert_eq!(quoted(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(quoted("t\tn\nr\r"), r#""t\tn\nr\r""#);
        assert_eq!(quoted("\u{0}\u{1f}\u{7f}"), r#""\u0000\u001f\u007f""#);
        assert_eq!(quoted("\u{8}\u{c}"), r#""\b\f""#);
        assert_eq!(quoted("\u{2028}"), "\"\\u2028\"");
    }

    #[test]
    fn objects() {
        let inner = Object::new().field("x", 1u64).finish();
        let s = Object::new()
            .field("s", "v")
            .field("n", 42u64)
            .field("f", Value::F64(0.5))
            .field("nan", Value::F64(f64::NAN))
            .field("b", true)
            .field("none", None::<&str>)
            .field("some", Some("y"))
            .field("o", Value::Raw(&inner))
            .field("a", Value::Raw(&array(vec!["1".into(), "2".into()])))
            .finish();
        assert_eq!(
            s,
            r#"{"s":"v","n":42,"f":0.5,"nan":null,"b":true,"none":null,"some":"y","o":{"x":1},"a":[1,2]}"#
        );
        assert_eq!(Object::new().finish(), "{}");
        assert_eq!(array(Vec::new()), "[]");
    }
}
