//! Whether the mail provider could verify where a message came from, read
//! from the Authentication-Results header (RFC 8601) its receiving server
//! added. Only the topmost one counts: anything below it came with the
//! message, and the sender can write whatever they like there.
//!
//! Only a failure is acted on. A forged "pass" could otherwise earn a
//! spoofed message a badge of trust, where a forged "fail" only hurts the
//! one who forged it.

/// What the provider concluded about the sender.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Verdict {
    #[default]
    Unknown,
    Pass,
    /// DMARC failed, or SPF failed with no passing DKIM signature: the
    /// From address may not be who sent it.
    Fail,
}

/// The verdict of the topmost of a message's Authentication-Results
/// headers, given top to bottom.
pub fn verdict<'a>(headers: impl IntoIterator<Item = &'a str>) -> Verdict {
    let Some(topmost) = headers.into_iter().next() else {
        return Verdict::Unknown;
    };
    let mut dmarc = None;
    let mut spf = None;
    let mut dkim = None;
    // The first part is the server that checked (authserv-id) -- except
    // from Exchange Online, which leaves it out and starts with a result.
    // Each other part is `method=result` and properties; the first result
    // for each method is the one that counts.
    for part in split_unquoted(&strip_comments(topmost), ';') {
        let Some((method, rest)) = part.trim().split_once('=') else {
            continue;
        };
        let result = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let slot = match method.trim().to_ascii_lowercase().as_str() {
            "dmarc" => &mut dmarc,
            "spf" => &mut spf,
            "dkim" => &mut dkim,
            _ => continue,
        };
        // Several DKIM signatures: any one passing is a pass.
        if slot.is_none() || (method.trim().eq_ignore_ascii_case("dkim") && result == "pass") {
            *slot = Some(result);
        }
    }
    match (dmarc.as_deref(), spf.as_deref(), dkim.as_deref()) {
        (Some("pass"), _, _) => Verdict::Pass,
        (Some("fail"), _, _) => Verdict::Fail,
        (_, Some("fail"), dkim) if dkim != Some("pass") => Verdict::Fail,
        (_, Some("pass"), Some("pass")) => Verdict::Pass,
        _ => Verdict::Unknown,
    }
}

/// Drop RFC 5322 comments, "(like this)", which may hold anything.
fn strip_comments(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    let mut in_quotes = false;
    for c in text.chars() {
        match c {
            '"' if depth == 0 => {
                in_quotes = !in_quotes;
                out.push(c);
            }
            '(' if !in_quotes => depth += 1,
            ')' if !in_quotes && depth > 0 => depth -= 1,
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn split_unquoted(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (index, c) in text.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c == separator && !in_quotes {
            parts.push(&text[start..index]);
            start = index + c.len_utf8();
        }
    }
    parts.push(&text[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_topmost_header_only() {
        let gmail = "mx.google.com;\r\n dkim=pass header.i=@example.com header.s=s1;\r\n spf=pass (google.com: domain of a@example.com designates 1.2.3.4 as permitted sender) smtp.mailfrom=a@example.com;\r\n dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=example.com";
        assert_eq!(verdict([gmail]), Verdict::Pass);
        let forged = "evil.example; dmarc=pass header.from=bank.example";
        let failed = "mx.google.com; spf=fail smtp.mailfrom=bank.example; dmarc=fail (p=NONE) header.from=bank.example";
        // The provider's header is on top; the forged one below doesn't count.
        assert_eq!(verdict([failed, forged]), Verdict::Fail);
        assert_eq!(verdict(Vec::<&str>::new()), Verdict::Unknown);
    }

    #[test]
    fn spf_failure_needs_no_passing_signature() {
        let outlook = "spf=fail (sender IP is 5.6.7.8) smtp.mailfrom=x.example; dkim=none (message not signed) header.d=none;dmarc=none action=none header.from=x.example;";
        // Exchange Online omits the authserv-id; its first part is a method.
        assert_eq!(verdict([outlook]), Verdict::Fail);
        let with_id = format!("protection.outlook.com; {outlook}");
        assert_eq!(verdict([with_id.as_str()]), Verdict::Fail);
        let signed = "mx.example; spf=fail smtp.mailfrom=x; dkim=fail; dkim=pass header.d=x";
        assert_eq!(verdict([signed]), Verdict::Unknown);
    }

    #[test]
    fn comments_may_hold_separators() {
        let header = "mx.example; dmarc=fail (p=quarantine; dis=none) header.from=x.example";
        assert_eq!(verdict([header]), Verdict::Fail);
    }
}
