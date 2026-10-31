// SPDX-License-Identifier: GPL-3.0-only
//! The token file ([`crate::path::TOKEN_PATH`]): one token per line, blank lines and `#` comments ignored.
//! A file holding a single token, as `kickstand token` used to set up, reads as one entry.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{MAX_TOKENS, valid_token};

/// The tokens in `text`, in file order.
pub fn parse(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddError {
    Invalid,
    Full,
}

/// `text` with `token` appended under a comment naming `who` (the PC's address), or `None` if it's
/// already there. The user's lines are kept as they are.
pub fn add(text: &str, token: &str, who: &str) -> Result<Option<String>, AddError> {
    if !valid_token(token) {
        return Err(AddError::Invalid);
    }
    let tokens = parse(text);
    if tokens.contains(&token) {
        return Ok(None);
    }
    if tokens.len() >= MAX_TOKENS {
        return Err(AddError::Full);
    }
    let mut out = String::from(text);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    // Keep the comment to one line whatever `who` holds.
    let who: String = who.chars().filter(|c| !c.is_control()).collect();
    out.push_str("# paired: ");
    out.push_str(&who);
    out.push('\n');
    out.push_str(token);
    out.push('\n');
    Ok(Some(out))
}

/// `text` without `token`, nor the `# paired:` comment [`add`] put above it, or `None` if it isn't there.
/// The other lines are kept as they are.
pub fn remove(text: &str, token: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut drop = alloc::vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == token {
            drop[i] = true;
            if i > 0 && lines[i - 1].trim_start().starts_with("# paired:") {
                drop[i - 1] = true;
            }
        }
    }
    if !drop.contains(&true) {
        return None;
    }
    let mut out = String::new();
    for (line, _) in lines.iter().zip(&drop).filter(|(_, d)| !**d) {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

/// Compares in time independent of where the first difference is.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0123456789abcdef0123456789abcdef";
    const B: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn single_token_files_still_work() {
        assert_eq!(parse(&alloc::format!("  {A}  \n")), [A]);
        assert_eq!(parse(A), [A]);
    }

    #[test]
    fn adds_once_and_keeps_the_users_lines() {
        let added = add(A, B, "192.168.1.5").unwrap().unwrap();
        assert_eq!(added, alloc::format!("{A}\n# paired: 192.168.1.5\n{B}\n"));
        assert_eq!(parse(&added), [A, B]);
        assert_eq!(add(&added, B, "x"), Ok(None));
        assert_eq!(add("", "short", "x"), Err(AddError::Invalid));
        assert_eq!(add("", &alloc::format!("{A}\nevil"), "x"), Err(AddError::Invalid));
        assert_eq!(add("", A, "a\nb").unwrap().unwrap(), alloc::format!("# paired: ab\n{A}\n"));
    }

    #[test]
    fn removes_a_token_and_its_comment() {
        let file = alloc::format!("# mine\n{A}\n# paired: 192.168.1.5\n{B}\n");
        assert_eq!(remove(&file, B).unwrap(), alloc::format!("# mine\n{A}\n"));
        // A token of the user's own keeps the comment above it, which isn't one of ours.
        assert_eq!(remove(&file, A).unwrap(), alloc::format!("# mine\n# paired: 192.168.1.5\n{B}\n"));
        assert_eq!(remove(&file, "0123"), None);
        assert_eq!(remove(&alloc::format!("  {A}  "), A).unwrap(), "");
        let added = add("", A, "x").unwrap().unwrap();
        assert_eq!(parse(&remove(&added, A).unwrap()), [] as [&str; 0]);
    }

    #[test]
    fn refuses_past_the_limit() {
        let full: String = (0..MAX_TOKENS).map(|i| alloc::format!("{A}{i:02}\n")).collect();
        assert_eq!(add(&full, B, "x"), Err(AddError::Full));
    }

    #[test]
    fn compares() {
        assert!(same(A, A));
        assert!(!same(A, B));
        assert!(!same(A, "0123"));
    }
}
