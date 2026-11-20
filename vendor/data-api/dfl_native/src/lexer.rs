//! Minimal lexer for DataLens formulas.
//!
//! Supports the subset of the grammar needed to demonstrate the native pipeline:
//! `[field]` references, numeric/string/date literals, comments, arithmetic/comparison/
//! logical operators, `IF ... THEN ... ELSEIF ... ELSE ... END`, `CASE ... WHEN ... END`,
//! function calls with the extension clauses of `DataLens.g4`
//! (`WITHIN`/`AMONG`/`TOTAL`, `ORDER BY`, LOD, `BEFORE FILTER BY`, `IGNORE DIMENSIONS`).
//!
//! Token boundaries follow `DataLens.g4`, because a formula the native parser *rejects* has
//! to be rejected by `dl_formula` too: a field name is `[` + at least one character that is
//! not `[`, `]`, `{` or `}` + `]` (no escaping at all), a number is `INT`, `FLOAT` or `INT
//! EXP`, a string escapes its quote with a backslash, and `--`/`/* */` comments are skipped.
//!
//! Three details decide whether the *error* matches, so they are modelled here and not in the
//! parser: `UNEXPECTED_CHARACTER: .` (a character no rule can start a token with becomes a
//! token of its own and scanning continues), `'#'`/`'##'` separated from `DATE_INNER`/
//! `DATETIME_INNER`, and the multi-word clause tokens, whose words spell one token only when
//! they are separated by spaces (`ORDER BY`, `BEFORE FILTER BY`, `IGNORE DIMENSIONS`).

/// Byte range of one token in the source formula, `[start, end)`.
///
/// `dl_formula` reports error positions as character offsets into the formula
/// (`FormulaErrorCtx.position.start`/`.end`, both equal to the offset of the offending
/// token), and the offending token as raw source text (`FormulaErrorCtx.token`), so the
/// tokenizer has to keep the ranges around.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

/// Token stream plus the source range of every token (same order, same length).
#[derive(Debug, Default)]
pub struct Lexed {
    pub toks: Vec<Tok>,
    pub spans: Vec<Span>,
}

impl Lexed {
    fn push(&mut self, tok: Tok, start: usize, end: usize) {
        self.toks.push(tok);
        self.spans.push(Span {
            start: start as u32,
            end: end as u32,
        });
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Field(Box<str>),
    /// A numeric literal, with its source lexeme.
    ///
    /// `dl_formula` keeps integers as `int` (arbitrary precision) and renders floats with
    /// Python's `repr`, so parsing to `f64` here would round large integers and change the
    /// text of small/large floats. The lexeme is the only lossless representation; the
    /// numeric value is parsed where it is actually needed (`codegen::format_number`).
    Number {
        text: Box<str>,
        is_int: bool,
    },
    Str(Box<str>),
    /// `'#'` (1) or `'##'` (2) of a date literal, a token of its own in `DataLens.g4`.
    Hash(u8),
    /// `DATE_INNER`/`DATETIME_INNER`: the text between the hashes of a date literal.
    ///
    /// Both are separate tokens in the grammar (`dateLiteral: '#' DATE_INNER '#'`), so a
    /// broken literal fails in the parser at the token that does not fit, not here.
    DateInner {
        text: Box<str>,
        is_datetime: bool,
    },
    /// `UNEXPECTED_CHARACTER: .` — a character no other rule can start a token with.
    ///
    /// The grammar has this catch-all rule, so a malformed token never fails the lexing as a
    /// whole: ANTLR emits the offending character as a one-character token and keeps
    /// scanning, and the parser then fails on it (`FormulaErrorCtx.token` is that character).
    Unexpected,
    Ident(Box<str>), // function name or keyword (uppercased)
    Plus,
    Minus,
    Mul,
    Div,
    Mod,
    Caret,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    LParen,
    RParen,
    Comma,
    Eof,
}

/// Length of the `DATE_INNER`/`DATETIME_INNER` match at the start of `src`, together with
/// whether it carries a time part.
///
/// The two tokens share the `DDDD '-' DD '-' DD` prefix; `DATETIME_INNER` adds
/// `[Tt ] DD (':' DD (':' DD)?)?` or a lone `[Tt]`. A *space* on its own is therefore not part
/// of either token (`DATE_INNER` ends at the day and ANTLR skips the space as `WS`), while a
/// lone `T` is: `#2019-01-01T#` is a datetime. ANTLR keeps the longest match, so a truncated
/// time part (`2020-01-01 10:`) still ends the token after the hour.
///
/// Only the shape is checked here; whether the date exists is validated by the parser, as in
/// `dl_formula`'s visitor.
fn date_literal_len(src: &[u8]) -> Option<(usize, bool)> {
    fn time_len(src: &[u8]) -> Option<usize> {
        // `DD`, `DD ':' DD`, `DD ':' DD ':' DD`
        let digit = |idx: usize| src.get(idx).is_some_and(u8::is_ascii_digit);
        if !(digit(0) && digit(1)) {
            return None;
        }
        if src.get(2) == Some(&b':') && digit(3) && digit(4) {
            if src.get(5) == Some(&b':') && digit(6) && digit(7) {
                return Some(8);
            }
            return Some(5);
        }
        Some(2)
    }

    let digit = |idx: usize| src.get(idx).is_some_and(u8::is_ascii_digit);
    let dash = |idx: usize| src.get(idx) == Some(&b'-');
    let is_date = digit(0)
        && digit(1)
        && digit(2)
        && digit(3)
        && dash(4)
        && digit(5)
        && digit(6)
        && dash(7)
        && digit(8)
        && digit(9);
    if !is_date {
        return None;
    }
    match src.get(10) {
        None => Some((10, false)),
        Some(b'T') | Some(b't') => Some((11 + time_len(&src[11..]).unwrap_or(0), true)),
        Some(b' ') => match time_len(&src[11..]) {
            Some(len) => Some((11 + len, true)),
            None => Some((10, false)),
        },
        Some(_) => Some((10, false)),
    }
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
        }
    }

    /// `WS: [ \t\n\r\f\u00a0]+ -> skip` from `DataLens.g4`.
    ///
    /// The scan is byte-wise, so `\u00a0` is matched on its two-byte UTF-8 encoding. Casting a
    /// single byte to `char` would turn `\u00a0` into `Â` (its leading `0xc2`), which the
    /// grammar does not accept, and a formula written with a non-breaking space would fail to
    /// parse even though ANTLR reads it as whitespace.
    fn skip_ws(&mut self) {
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' | 0x0c => self.pos += 1,
                0xc2 if self.src.get(self.pos + 1) == Some(&0xa0) => self.pos += 2,
                _ => break,
            }
        }
    }

    fn rest_starts_with(&self, prefix: &[u8]) -> bool {
        self.src[self.pos..].starts_with(prefix)
    }

    /// Skip `-- ...` and `/* ... */`, which `DataLens.g4` drops before the parser sees them.
    ///
    /// An unterminated block comment is left in place: ANTLR cannot build the comment token,
    /// so `/` and `*` are lexed with their own rules and the parser fails on the `/`.
    fn skip_comments(&mut self) {
        loop {
            if self.rest_starts_with(b"--") {
                // `SINGLE_LINE_COMMENT: '--' ~[\n]* ('\n'|EOF) -> skip`
                while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else if self.rest_starts_with(b"/*") {
                let open = self.pos;
                self.pos += 2;
                loop {
                    if self.pos + 1 >= self.src.len() {
                        self.pos = open;
                        return;
                    }
                    if self.src[self.pos] == b'*' && self.src[self.pos + 1] == b'/' {
                        self.pos += 2;
                        break;
                    }
                    self.pos += 1;
                }
            } else {
                return;
            }
            self.skip_ws();
        }
    }

    /// Tokenize the whole formula. A malformed token never stops the scan: it becomes
    /// `Tok::Unexpected` and the lexer continues right after it, exactly like ANTLR's
    /// `UNEXPECTED_CHARACTER` rule.
    pub fn tokenize(mut self) -> Lexed {
        let mut out = Lexed {
            toks: Vec::with_capacity(16),
            spans: Vec::with_capacity(16),
        };
        loop {
            self.skip_ws();
            self.skip_comments();
            self.skip_ws();
            let start = self.pos;
            if self.pos >= self.src.len() {
                out.push(Tok::Eof, start, start);
                return out;
            }
            let tok = self.lex_one(start);
            out.push(tok, start, self.pos);
        }
    }

    /// The token at `self.pos`, with `self.pos` left at its end.
    fn lex_one(&mut self, start: usize) -> Tok {
        // `DATE_INNER`/`DATETIME_INNER` win over the number rules: ANTLR keeps the longest
        // match, and `2020-01-01` is one token, not `2020`, `-`, `01`, `-`, `01`.
        if let Some((len, is_datetime)) = date_literal_len(&self.src[start..]) {
            let text = String::from_utf8_lossy(&self.src[start..start + len]).to_string();
            self.pos = start + len;
            return Tok::DateInner {
                text: text.into_boxed_str(),
                is_datetime,
            };
        }
        let ch = self.current_char().unwrap_or('\0');
        match ch {
            '[' => self.lex_field(),
            '\'' | '"' => self.lex_string(ch as u8),
            '0'..='9' => self.lex_number(),
            // `'#'` and `'##'` are tokens of their own, `DATE_INNER` follows them.
            '#' => {
                if self.src.get(start + 1) == Some(&b'#') {
                    self.pos = start + 2;
                    Tok::Hash(2)
                } else {
                    self.pos = start + 1;
                    Tok::Hash(1)
                }
            }
            '.' if matches!(self.src.get(start + 1), Some(b) if b.is_ascii_digit()) => {
                self.lex_number()
            }
            _ if ch.is_ascii_alphabetic() || ch == '_' => self.lex_ident(),
            _ => self.lex_op(ch),
        }
    }

    /// The character at `self.pos`, if the position is inside the formula.
    fn current_char(&self) -> Option<char> {
        std::str::from_utf8(&self.src[self.pos..])
            .ok()?
            .chars()
            .next()
    }

    /// `UNEXPECTED_CHARACTER: .` at `at`: ANTLR emits the character alone and keeps scanning.
    fn unexpected(&mut self, at: usize) -> Tok {
        let len = std::str::from_utf8(&self.src[at..])
            .ok()
            .and_then(|rest| rest.chars().next())
            .map_or(1, char::len_utf8);
        self.pos = at + len;
        Tok::Unexpected
    }

    /// `FIELD_NAME: '[' ~[[\]{}]+ ']'` — no escaping, and at least one character inside.
    ///
    /// When no field name can be built, ANTLR emits the `[` as an unexpected character and
    /// keeps scanning, so the reported token is that `[`.
    fn lex_field(&mut self) -> Tok {
        let open = self.pos;
        let start = self.pos + 1;
        let mut i = start;
        let mut end = None;
        while i < self.src.len() {
            match self.src[i] {
                b']' => {
                    end = Some(i);
                    break;
                }
                b'[' | b'{' | b'}' => break,
                _ => i += 1,
            }
        }
        match end {
            Some(end) if end > start => {
                let name = String::from_utf8_lossy(&self.src[start..end]).to_string();
                self.pos = end + 1;
                Tok::Field(name.into_boxed_str())
            }
            _ => self.unexpected(open),
        }
    }

    /// `ESCAPED_STRING: '\'' ( '\\\'' | ~['] )* '\'' | '"' ( '\\"' | ~["] )* '"'`
    ///
    /// Escapes are a backslash followed by the quote: `'\''` is a single-quote character, not
    /// the end of the literal. A literal that is never closed yields the opening quote as the
    /// unexpected character, exactly like ANTLR.
    ///
    /// The body is collected as bytes and decoded once: pushing each byte into a `char` would
    /// turn every multi-byte character (`'Привет'`, a non-breaking space) into mojibake. The
    /// scan is still byte-wise because a closing quote is ASCII and can never be a UTF-8
    /// continuation byte.
    fn lex_string(&mut self, quote: u8) -> Tok {
        let open = self.pos;
        let mut i = self.pos + 1;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            if i >= self.src.len() {
                return self.unexpected(open);
            }
            let b = self.src[i];
            if b == b'\\' && self.src.get(i + 1) == Some(&quote) {
                buf.push(quote);
                i += 2;
                continue;
            }
            if b == quote {
                break;
            }
            buf.push(b);
            i += 1;
        }
        self.pos = i + 1;
        Tok::Str(String::from_utf8_lossy(&buf).into_owned().into_boxed_str())
    }

    /// `INT`, `FLOAT` (`INT EXP | DECIMAL EXP?`, `DECIMAL: INT '.' INT? | '.' INT`).
    ///
    /// Every scan stops at the longest prefix the grammar accepts, so `1.2.3` is two tokens
    /// (`1.2` and `.3`) and the parser reports the second one, as `dl_formula` does.
    fn lex_number(&mut self) -> Tok {
        let start = self.pos;
        let mut i = self.pos;
        let digits = |i: &mut usize| {
            while *i < self.src.len() && self.src[*i].is_ascii_digit() {
                *i += 1;
            }
        };
        let exponent = |i: &mut usize| {
            // `EXP: [eE] [+-]? INT`
            if matches!(self.src.get(*i), Some(b'e') | Some(b'E')) {
                let mut j = *i + 1;
                if matches!(self.src.get(j), Some(b'+') | Some(b'-')) {
                    j += 1;
                }
                let digits_at = j;
                while j < self.src.len() && self.src[j].is_ascii_digit() {
                    j += 1;
                }
                if j > digits_at {
                    *i = j;
                }
            }
        };
        digits(&mut i);
        let int_digits = i > start;
        let mut is_int = true;
        if int_digits {
            if self.src.get(i) == Some(&b'.') {
                // `DECIMAL: INT '.' INT?`
                i += 1;
                digits(&mut i);
                is_int = false;
            }
            let before_exp = i;
            exponent(&mut i);
            if i > before_exp {
                is_int = false;
            }
        } else if self.src.get(i) == Some(&b'.') {
            // `DECIMAL: '.' INT`
            i += 1;
            digits(&mut i);
            is_int = false;
        }
        let text = std::str::from_utf8(&self.src[start..i]).unwrap_or("0");
        self.pos = i;
        Tok::Number {
            text: text.into(),
            is_int,
        }
    }

    fn lex_ident(&mut self) -> Tok {
        let start = self.pos;
        while self.pos < self.src.len() {
            let b = self.src[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
        Tok::Ident(text.to_ascii_uppercase().into_boxed_str())
    }

    fn lex_op(&mut self, ch: char) -> Tok {
        self.pos += ch.len_utf8();
        match ch {
            '+' => Tok::Plus,
            '-' => Tok::Minus,
            '*' => Tok::Mul,
            '/' => Tok::Div,
            '%' => Tok::Mod,
            '^' => Tok::Caret,
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            ',' => Tok::Comma,
            '=' => Tok::Eq,
            '!' if self.src.get(self.pos) == Some(&b'=') => {
                self.pos += 1;
                Tok::Neq
            }
            '<' => match self.src.get(self.pos) {
                Some(b'=') => {
                    self.pos += 1;
                    Tok::Lte
                }
                Some(b'>') => {
                    self.pos += 1;
                    Tok::Neq
                }
                _ => Tok::Lt,
            },
            '>' => {
                if self.src.get(self.pos) == Some(&b'=') {
                    self.pos += 1;
                    Tok::Gte
                } else {
                    Tok::Gt
                }
            }
            other => {
                // `UNEXPECTED_CHARACTER: .` — ANTLR reports this very character.
                self.pos -= other.len_utf8();
                self.unexpected(self.pos)
            }
        }
    }
}
