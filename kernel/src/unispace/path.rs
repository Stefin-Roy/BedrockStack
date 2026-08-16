use alloc::vec::Vec;

use super::UnispaceError;

/// A parsed unispace path: a sequence of components plus an optional trailing
/// `:method` selector on the final component.  `/a/b/c:m` → components
/// `["a","b","c"]`, method `Some("m")`.  `/` (or `""`) → no components.
pub struct ParsedPath<'a> {
    pub components: Vec<&'a str>,
    pub method: Option<&'a str>,
}

/// Parse a unispace path.
///
/// Rules:
/// - Components are separated by `/`.  A leading `/` is optional; the namespace
///   is always resolved from the root, so `/a` and `a` are identical.
/// - Empty components (`//`) are ignored.
/// - Only the final component may carry a `:method` selector; `:` inside any
///   earlier component or an empty method name is `InvalidPath`.
/// - `.` and `..` are rejected (the namespace has no relative semantics).
pub fn parse(path: &str) -> Result<ParsedPath<'_>, UnispaceError> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Ok(ParsedPath {
            components: Vec::new(),
            method: None,
        });
    }

    let mut raw: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if raw.is_empty() {
        return Ok(ParsedPath {
            components: Vec::new(),
            method: None,
        });
    }

    let mut method = None;
    if let Some(last) = raw.pop() {
        if let Some(pos) = last.find(':') {
            if pos == 0 {
                return Err(UnispaceError::InvalidPath);
            }
            let name = &last[..pos];
            let sel = &last[pos + 1..];
            if name.is_empty() || sel.is_empty() {
                return Err(UnispaceError::InvalidPath);
            }
            raw.push(name);
            method = Some(sel);
        } else {
            raw.push(last);
        }
    }

    for seg in &raw {
        if *seg == "." || *seg == ".." || seg.contains(':') {
            return Err(UnispaceError::InvalidPath);
        }
    }

    Ok(ParsedPath {
        components: raw,
        method,
    })
}
