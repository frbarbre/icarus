use std::fmt;

use crate::json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct KeyPath(Vec<String>);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("the key is empty")]
    Empty,
    #[error("`{0}` has an empty segment")]
    EmptySegment(String),
    #[error("`{key}`: `{parent}` is a {found}, not an object or array")]
    NotContainer {
        key: String,
        parent: String,
        found: &'static str,
    },
    #[error("`{key}`: `{segment}` is not a valid index into an array of {len}")]
    BadIndex {
        key: String,
        segment: String,
        len: usize,
    },
}

impl KeyPath {
    // A backslash escapes a dot, so keys like `editor-dark.json` stay addressable.
    pub fn parse(key: &str) -> Result<Self, KeyError> {
        if key.is_empty() {
            return Err(KeyError::Empty);
        }
        let mut segments = Vec::new();
        let mut current = String::new();
        let mut chars = key.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => current.push(chars.next().unwrap_or('\\')),
                '.' => segments.push(std::mem::take(&mut current)),
                c => current.push(c),
            }
        }
        segments.push(current);
        if segments.iter().any(String::is_empty) {
            return Err(KeyError::EmptySegment(key.to_owned()));
        }
        Ok(Self(segments))
    }

    pub fn from_segments<I: IntoIterator<Item = S>, S: Into<String>>(segments: I) -> Self {
        Self(segments.into_iter().map(Into::into).collect())
    }

    pub fn segments(&self) -> &[String] {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn child(&self, segment: impl Into<String>) -> Self {
        let mut next = self.0.clone();
        next.push(segment.into());
        Self(next)
    }

    fn prefix(&self, len: usize) -> String {
        Self(self.0[..len].to_vec()).to_string()
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(&segment.replace('\\', "\\\\").replace('.', "\\."))?;
        }
        Ok(())
    }
}

fn step<'a>(value: &'a Value, segment: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map.get(segment),
        Value::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => None,
    }
}

pub fn get<'a>(doc: &'a Value, path: &KeyPath) -> Option<&'a Value> {
    path.0
        .iter()
        .try_fold(doc, |value, segment| step(value, segment))
}

pub fn set(doc: &mut Value, path: &KeyPath, new_value: Value) -> Result<(), KeyError> {
    let Some((last, parents)) = path.0.split_last() else {
        *doc = new_value;
        return Ok(());
    };
    let mut current = doc;
    for (depth, segment) in parents.iter().enumerate() {
        if current.is_null() {
            *current = Value::object();
        }
        current = match current {
            Value::Object(map) => map.entry(segment.clone()).or_insert_with(Value::object),
            Value::Array(items) => {
                let len = items.len();
                let index = parse_index(path, segment, len, false)?;
                &mut items[index]
            }
            other => {
                return Err(KeyError::NotContainer {
                    key: path.to_string(),
                    parent: path.prefix(depth),
                    found: other.kind(),
                });
            }
        };
    }
    if current.is_null() {
        *current = Value::object();
    }
    match current {
        Value::Object(map) => {
            map.insert(last.clone(), new_value);
        }
        Value::Array(items) => {
            let len = items.len();
            let index = parse_index(path, last, len, true)?;
            if index == len {
                items.push(new_value);
            } else {
                items[index] = new_value;
            }
        }
        other => {
            return Err(KeyError::NotContainer {
                key: path.to_string(),
                parent: path.prefix(parents.len()),
                found: other.kind(),
            });
        }
    }
    Ok(())
}

fn parse_index(
    path: &KeyPath,
    segment: &str,
    len: usize,
    allow_end: bool,
) -> Result<usize, KeyError> {
    segment
        .parse::<usize>()
        .ok()
        .filter(|&i| i < len || (allow_end && i == len))
        .ok_or_else(|| KeyError::BadIndex {
            key: path.to_string(),
            segment: segment.to_owned(),
            len,
        })
}

pub fn unset(doc: &mut Value, path: &KeyPath) -> Option<Value> {
    let (last, parents) = path.0.split_last()?;
    let mut current = doc;
    for segment in parents {
        current = match current {
            Value::Object(map) => map.get_mut(segment)?,
            Value::Array(items) => items.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    match current {
        Value::Object(map) => map.shift_remove(last),
        Value::Array(items) => {
            let index = last.parse::<usize>().ok().filter(|&i| i < items.len())?;
            Some(items.remove(index))
        }
        _ => None,
    }
}

pub fn parse_cli_value(text: &str) -> Value {
    crate::json::parse(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}
