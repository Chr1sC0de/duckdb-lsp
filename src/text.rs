use tower_lsp::lsp_types::{Position, Range};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Word,
    Quoted,
    String,
    Comment,
    Number,
    Punct,
    Template,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}
impl Token {
    pub fn name(&self) -> String {
        if self.kind == Kind::Quoted {
            self.text.trim_matches('"').replace("\"\"", "\"")
        } else {
            self.text.clone()
        }
    }
    pub fn is(&self, s: &str) -> bool {
        self.text.eq_ignore_ascii_case(s)
    }
    pub fn ident(&self) -> bool {
        matches!(self.kind, Kind::Word | Kind::Quoted)
    }
}

pub fn lex(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        let kind;
        if s[i..].starts_with("--") {
            i = s[i..].find('\n').map(|n| i + n).unwrap_or(s.len());
            kind = Kind::Comment;
        } else if s[i..].starts_with("/*") {
            i += 2;
            let mut depth = 1;
            while i < s.len() && depth > 0 {
                if s[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if s[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += s[i..].chars().next().unwrap().len_utf8();
                }
            }
            kind = Kind::Comment;
        } else if ["{{", "{%", "{#"].iter().any(|x| s[i..].starts_with(x)) {
            let close = match &s[i..i + 2] {
                "{{" => "}}",
                "{%" => "%}",
                _ => "#}",
            };
            i = s[i + 2..]
                .find(close)
                .map(|n| i + 2 + n + 2)
                .unwrap_or(s.len());
            kind = Kind::Template;
        } else if c == '\'' || c == '"' {
            kind = if c == '\'' {
                Kind::String
            } else {
                Kind::Quoted
            };
            i += 1;
            let escape = start > 0 && matches!(s.as_bytes()[start - 1], b'e' | b'E');
            while i < s.len() {
                let ch = s[i..].chars().next().unwrap();
                i += ch.len_utf8();
                if ch == c {
                    if s[i..].starts_with(c) {
                        i += 1;
                    } else {
                        break;
                    }
                } else if ch == '\\' && escape && i < s.len() {
                    i += s[i..].chars().next().unwrap().len_utf8();
                }
            }
        } else if c == '$' && dollar_tag(&s[i..]).is_some() {
            let tag = dollar_tag(&s[i..]).unwrap();
            i = s[i + tag.len()..]
                .find(tag)
                .map(|n| i + 2 * tag.len() + n)
                .unwrap_or(s.len());
            kind = Kind::String;
        } else if c.is_alphabetic() || c == '_' {
            i += c.len_utf8();
            while i < s.len() {
                let c = s[i..].chars().next().unwrap();
                if c.is_alphanumeric() || c == '_' || c == '$' {
                    i += c.len_utf8();
                } else {
                    break;
                }
            }
            kind = Kind::Word;
        } else if c.is_ascii_digit() {
            i += 1;
            while i < s.len() && s.as_bytes()[i].is_ascii_digit() {
                i += 1;
            }
            kind = Kind::Number;
        } else {
            i += c.len_utf8();
            kind = Kind::Punct;
        }
        out.push(Token {
            text: s[start..i].into(),
            start,
            end: i,
            kind,
        });
    }
    out
}
fn dollar_tag(s: &str) -> Option<&str> {
    let end = s[1..].find('$')? + 1;
    if s[1..end].chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(&s[..end + 1])
    } else {
        None
    }
}
pub fn position(s: &str, byte: usize) -> Position {
    let mut byte = byte.min(s.len());
    while !s.is_char_boundary(byte) {
        byte -= 1;
    }
    let prefix = &s[..byte];
    let line = prefix.bytes().filter(|b| *b == b'\n').count();
    let start = prefix.rfind('\n').map(|n| n + 1).unwrap_or(0);
    Position::new(line as u32, s[start..byte].encode_utf16().count() as u32)
}
pub fn offset(s: &str, p: Position) -> usize {
    let mut line = 0;
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if line == p.line && (units >= p.character || c == '\n' || c == '\r') {
            return i;
        }
        if c == '\n' {
            line += 1;
            units = 0;
        } else if line == p.line {
            units += c.len_utf16() as u32;
        }
    }
    s.len()
}
pub fn range(s: &str, start: usize, end: usize) -> Range {
    Range::new(position(s, start), position(s, end))
}
pub fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
pub fn statements(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for t in lex(s) {
        if t.text == ";" && t.kind == Kind::Punct {
            out.push((start, t.start));
            start = t.end;
        }
    }
    if start < s.len() {
        out.push((start, s.len()));
    }
    out
}
pub fn statement_at(s: &str, byte: usize) -> (usize, usize) {
    statements(s)
        .into_iter()
        .find(|(a, b)| *a <= byte && byte <= *b)
        .unwrap_or((0, s.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_roundtrip() {
        let s = "-- 🦆\r\nSELECT café";
        for (i, _) in s.char_indices().filter(|(_, c)| *c != '\n') {
            assert_eq!(offset(s, position(s, i)), i);
        }
    }
    #[test]
    fn split_literals() {
        assert_eq!(
            statements("select ';', $$;$$; /* a /* ; */ b */ select 2").len(),
            2
        );
    }
}
