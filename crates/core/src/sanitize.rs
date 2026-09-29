//! Coerces arbitrary LLM output into the HTML subset Telegram's `parse_mode=HTML` accepts.
//! Port of the Python `text_sanitizer`, with `javascript:`-style links additionally dropped.

const KEEP: &[&str] = &["b", "strong", "i", "em", "u", "s", "strike", "code", "pre", "a", "tg-spoiler", "blockquote"];
const BLOCK: &[&str] = &["br", "p", "div", "li"];

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Is `s` (starting right after '&') a well-formed entity body like `amp;` or `#39;`?
fn is_entity(s: &str) -> bool {
    let Some(end) = s.find(';') else { return false };
    let body = &s[..end];
    match body.strip_prefix('#') {
        Some(n) => !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()),
        None => !body.is_empty() && body.chars().all(|c| c.is_ascii_alphanumeric()),
    }
}

/// Extracts `href` from a raw attribute string.
fn href(attrs: &str) -> Option<String> {
    let lower = attrs.to_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("href") {
        let at = from + i;
        let prev_ok = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = attrs[at + 4..].trim_start();
        if prev_ok && rest.starts_with('=') {
            let v = rest[1..].trim_start();
            let val = match v.chars().next() {
                Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or("").to_string(),
                _ => v.split_whitespace().next().unwrap_or("").to_string(),
            };
            return Some(val);
        }
        from = at + 4;
    }
    None
}

pub fn sanitize_html(input: &str) -> String {
    let raw = input.trim();
    if raw.len() >= 6 && raw.starts_with("```") && raw.ends_with("```") {
        let inner = &raw[3..raw.len() - 3];
        let body = inner.split_once('\n').map_or(inner, |(_, b)| b);
        return format!("<pre>{}</pre>", escape(body.trim_end_matches('\n')));
    }

    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let rest = &raw[i..];
        let ch = rest.chars().next().unwrap();
        match ch {
            '<' => match parse_tag(rest) {
                Some((len, closing, name, attrs)) => {
                    i += len;
                    if BLOCK.contains(&name.as_str()) {
                        newline(&mut out);
                    } else if KEEP.contains(&name.as_str()) {
                        let norm = match name.as_str() { "strong" => "b", "em" => "i", "strike" => "s", n => n };
                        if closing {
                            out.push_str(&format!("</{norm}>"));
                        } else if norm == "a" {
                            let safe = href(&attrs).filter(|h| {
                                let h = h.trim().to_lowercase();
                                ["http://", "https://", "tg://", "mailto:"].iter().any(|p| h.starts_with(p))
                            });
                            match safe {
                                Some(h) => out.push_str(&format!("<a href=\"{}\">", h.replace('"', "&quot;"))),
                                None => out.push_str("<a>"),
                            }
                        } else {
                            out.push_str(&format!("<{norm}>"));
                        }
                    }
                    continue;
                }
                None => out.push_str("&lt;"),
            },
            '>' => out.push_str("&gt;"),
            '&' => out.push_str(if is_entity(&rest[1..]) { "&" } else { "&amp;" }),
            c => out.push(c),
        }
        i += ch.len_utf8();
    }
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out.trim().to_string()
}

/// `<` + optional `/` + name + attributes + `>` → (byte length, closing?, lowercase name, raw attrs).
fn parse_tag(s: &str) -> Option<(usize, bool, String, String)> {
    let body = s.strip_prefix('<')?;
    let (closing, body) = body.strip_prefix('/').map_or((false, body), |b| (true, b));
    let name_len = body.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(body.len());
    if name_len == 0 || !body.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    let after = &body[name_len..];
    // attributes end at the first '>' not inside quotes
    let mut quote: Option<char> = None;
    let mut end = None;
    for (idx, c) in after.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => { end = Some(idx); break; }
            _ => {}
        }
    }
    let end = end?;
    let consumed = 1 + usize::from(closing) + name_len + end + 1;
    Some((consumed, closing, body[..name_len].to_lowercase(), after[..end].to_string()))
}

#[cfg(test)]
mod tests {
    use super::sanitize_html as s;

    #[test]
    fn normalizes_and_escapes() {
        assert_eq!(s("<strong>hi</strong> <em>x</em> <strike>y</strike>"), "<b>hi</b> <i>x</i> <s>y</s>");
        assert_eq!(s("a < b & c > d"), "a &lt; b &amp; c &gt; d");
        assert_eq!(s("<3 you"), "&lt;3 you");
        assert_eq!(s("&amp; &lt; &#39; &"), "&amp; &lt; &#39; &amp;");
        assert_eq!(s("unclosed <b"), "unclosed &lt;b");
        assert_eq!(s(""), "");
    }

    #[test]
    fn strips_unknown_tags_and_dangerous_links() {
        assert_eq!(s("<script>alert(1)</script>ok"), "alert(1)ok");
        assert_eq!(s("<a href=\"javascript:alert(1)\">x</a>"), "<a>x</a>");
        assert_eq!(s("<a href=\" JavaScript:alert(1)\">x</a>"), "<a>x</a>");
        assert_eq!(s("<a href=\"https://e.com/?a=1&b=2\" onclick=\"x\">l</a>"), "<a href=\"https://e.com/?a=1&b=2\">l</a>");
        assert_eq!(s("<a href='tg://user?id=1'>u</a>"), "<a href=\"tg://user?id=1\">u</a>");
        assert_eq!(s("<a>bare</a>"), "<a>bare</a>");
    }

    #[test]
    fn blocks_and_fences() {
        assert_eq!(s("line1<br>line2<p>line3"), "line1\nline2\nline3");
        assert_eq!(s("a\n\n\n\n\nb"), "a\n\nb");
        assert_eq!(s("```rust\nlet a = 1 < 2;\n```"), "<pre>let a = 1 &lt; 2;</pre>");
        assert_eq!(s("<b>Привіт</b>, 🌍 <i>світе</i>"), "<b>Привіт</b>, 🌍 <i>світе</i>");
    }
}
