//! Recursive-descent / Pratt parser for the formula subset.

use crate::ast::{BinOp, Clauses, Grouping, IsKind, Item, Lod, LodKind, OrderingDirection, UnOp};
use crate::func_props;
use crate::lexer::{Lexed, Lexer, Span, Tok};

/// `dl_formula`'s error codes, as `FormulaErrorCtx.code` joined with dots.
pub const UNEXPECTED_TOKEN: &str = "FORMULA.PARSE.UNEXPECTED_TOKEN";
pub const UNEXPECTED_EOF: &str = "FORMULA.PARSE.UNEXPECTED_EOF";
pub const EMPTY_FORMULA: &str = "FORMULA.PARSE.UNEXPECTED_EOF.EMPTY_FORMULA";
/// `dl_formula.core.exc.ParseDateValueError` / `ParseDatetimeValueError`: the literal has the
/// shape of a date, but no such date exists.
pub const DATE_VALUE: &str = "FORMULA.PARSE.VALUE.DATE";
pub const DATETIME_VALUE: &str = "FORMULA.PARSE.VALUE.DATETIME";
/// `dl_formula.core.exc.ParseClauseError`: a clause the function does not accept.
pub const CLAUSE: &str = "FORMULA.CLAUSE";

/// A rejected formula, shaped like `dl_formula.core.message_ctx.FormulaErrorCtx`.
///
/// The validator has to show the same messages the Python pipeline does, so the error keeps
/// the fields the API serialises: `code`, `message`, `token` and `position`. Positions are
/// character offsets (`start == end` for a point), as in `dl_formula.core.position.Position`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// `FormulaErrorCtx.code`, dot-joined (`FORMULA.PARSE.UNEXPECTED_EOF`, ...).
    pub code: &'static str,
    pub message: String,
    /// Text of the offending token (`FormulaErrorCtx.token`), when the error is about one.
    pub token: Option<String>,
    /// Offsets in the formula; `None` for errors `dl_formula` reports without a position.
    pub position: Option<Span>,
    /// `true` when `code`/`message`/`token`/`position` reproduce `dl_formula` exactly, so the
    /// text may be shown to the user. `false` means the error is a *diagnostic*: the formula is
    /// rejected, but for the user-facing message the Python parser has to be asked
    /// (`ANTLR` picks the offending token with its own error recovery, which a hand-written
    /// recursive-descent parser cannot reproduce for every malformed input).
    pub exact: bool,
}

impl ParseError {
    /// `dl_formula.core.exc.ParseUnexpectedEOFError`
    fn unexpected_eof(pos: u32) -> Self {
        Self {
            code: UNEXPECTED_EOF,
            message: "Failed to parse: unexpected end of formula".to_string(),
            token: None,
            position: Some(Span {
                start: pos,
                end: pos,
            }),
            exact: true,
        }
    }

    /// `dl_formula.core.exc.ParseEmptyFormulaError`
    fn empty_formula() -> Self {
        Self {
            code: EMPTY_FORMULA,
            message: "Empty formula".to_string(),
            token: None,
            position: Some(Span { start: 0, end: 0 }),
            exact: true,
        }
    }

    /// `dl_formula.parser.antlr.visitor.EMPTY_FORMULA_ERROR`: the text is not blank, but the
    /// lexer produced no tokens at all (it was nothing but comments), so ANTLR's `parse: EOF`
    /// branch fires and the visitor raises the same error *without* a position.
    fn empty_formula_without_tokens() -> Self {
        Self {
            code: EMPTY_FORMULA,
            message: "Empty formula is invalid".to_string(),
            token: None,
            position: None,
            exact: true,
        }
    }

    /// `dl_formula.core.exc.ParseUnexpectedTokenError`
    fn unexpected_token(text: &str, pos: u32) -> Self {
        Self {
            code: UNEXPECTED_TOKEN,
            message: "Failed to parse formula: unexpected token".to_string(),
            token: Some(text.to_string()),
            position: Some(Span {
                start: pos,
                end: pos,
            }),
            exact: true,
        }
    }

    /// `dl_formula.core.exc.ParseClauseError`: raised while post-processing the tree, so the
    /// message (and the absence of a position) is exactly what `dl_formula` produces.
    fn clause(message: String) -> Self {
        Self {
            code: CLAUSE,
            message,
            token: None,
            position: None,
            exact: true,
        }
    }

    /// `ParseDateValueError`/`ParseDatetimeValueError` of `dl_formula.core.exc`.
    ///
    /// `dl_formula` builds the node only after its value has been parsed, and raises with the
    /// literal text as the token and the whole literal (`#` included) as the position.
    fn invalid_date_value(text: &str, is_datetime: bool, span: Span) -> Self {
        Self {
            code: if is_datetime {
                DATETIME_VALUE
            } else {
                DATE_VALUE
            },
            message: format!(
                "Invalid {} value: {text}",
                if is_datetime { "datetime" } else { "date" }
            ),
            token: Some(text.to_string()),
            position: Some(span),
            exact: true,
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub struct Parser<'a> {
    /// The source formula: token text of an error is a slice of it.
    text: &'a str,
    lexed: Lexed,
    pos: usize,
}

impl<'a> Parser<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            lexed: Lexer::new(text).tokenize(),
            pos: 0,
        }
    }

    /// The multi-word tokens of the grammar, in the order their first word appears in.
    ///
    /// `DataLens.g4` makes `ORDER_BY`, `BEFORE_FILTER_BY` and `IGNORE_DIMENSIONS` single tokens
    /// with `' '+` between the words, so for the parser they are one token that happens to be
    /// spelled as several identifiers here.
    const MULTIWORD: [&'static [&'static str]; 3] = [
        &["ORDER", "BY"],
        &["BEFORE", "FILTER", "BY"],
        &["IGNORE", "DIMENSIONS"],
    ];

    /// Length of the multi-word token starting at `idx` (`ORDER BY` → 2 tokens), if the tokens
    /// there spell one of [`Parser::MULTIWORD`].
    fn multiword_len_at(&self, idx: usize) -> Option<usize> {
        Self::MULTIWORD
            .iter()
            .find_map(|words| self.phrase_len_from(idx, words))
    }

    /// Length of `words` at `idx`, if they are there and separated by runs of spaces.
    fn phrase_len_from(&self, idx: usize, words: &[&str]) -> Option<usize> {
        let found = words.iter().enumerate().all(|(offset, word)| {
            self.is_kw_at(idx + offset, word)
                && (offset == 0 || self.spaces_between(idx + offset - 1, idx + offset))
        });
        found.then_some(words.len())
    }

    fn peek(&self) -> &Tok {
        self.lexed.toks.get(self.pos).unwrap_or(&Tok::Eof)
    }

    fn next(&mut self) -> Tok {
        let tok = self.lexed.toks.get(self.pos).cloned().unwrap_or(Tok::Eof);
        self.pos += 1;
        tok
    }

    /// The error for the token at `idx`: `dl_formula` reports a missing token as
    /// `unexpected end of formula` and anything else as `unexpected token`.
    /// The error for the token at `idx`: `dl_formula` reports a missing token as
    /// `unexpected end of formula` and anything else as `unexpected token`.
    ///
    /// A multi-word token reports all of its words: `ORDER_BY` is one token of the grammar,
    /// so `[a] ORDER BY` fails on `ORDER BY` with the spaces it was written with.
    fn error_at(&self, idx: usize) -> ParseError {
        match self.lexed.toks.get(idx) {
            None | Some(Tok::Eof) => ParseError::unexpected_eof(self.text.len() as u32),
            Some(_) => {
                let first = self.lexed.spans[idx];
                let words = self.multiword_len_at(idx).unwrap_or(1);
                let last = self.lexed.spans[idx + words - 1];
                ParseError::unexpected_token(
                    &self.text[first.start as usize..last.end as usize],
                    first.start,
                )
            }
        }
    }

    fn error_here(&self) -> ParseError {
        self.error_at(self.pos)
    }

    fn eat(&mut self, expected: &Tok) -> bool {
        if self.peek() == expected {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &Tok) -> Result<(), ParseError> {
        if self.eat(expected) {
            Ok(())
        } else {
            Err(self.error_here())
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(name) if name.as_ref() == kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), ParseError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.error_here())
        }
    }

    /// `parse: expression EOF | EOF`
    pub fn parse_formula(&mut self) -> Result<Item, ParseError> {
        if self.text.trim().is_empty() {
            return Err(ParseError::empty_formula());
        }
        // Nothing but comments: ANTLR sees an empty token stream, not an empty string.
        if self.lexed.toks.len() == 1 {
            return Err(ParseError::empty_formula_without_tokens());
        }
        let item = self.parse_expr()?;
        if self.peek() != &Tok::Eof {
            return Err(self.error_here());
        }
        Ok(item)
    }

    fn parse_expr(&mut self) -> Result<Item, ParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Item, ParseError> {
        let mut left = self.parse_and()?;
        while self.eat_kw("OR") {
            let right = self.parse_and()?;
            left = Item::Binary(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Item, ParseError> {
        let mut left = self.parse_not()?;
        while self.eat_kw("AND") {
            let right = self.parse_not()?;
            left = Item::Binary(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Item, ParseError> {
        if self.eat_kw("NOT") {
            let inner = self.parse_not()?;
            return Ok(Item::Unary(UnOp::Not, Box::new(inner)));
        }
        self.parse_cmp()
    }

    /// `exprMain` of the grammar: `POWER`, `MULDIV` and `PLUS|MINUS` are already folded by
    /// `parse_add`, so what is left is the comparison layer — `IS`, `[NOT] LIKE`,
    /// `COMPARISON`, `[NOT] BETWEEN` and `[NOT] IN`.
    ///
    /// ANTLR builds these left-recursively over `exprMain`, so they chain
    /// (`a LIKE b LIKE c` is `(a LIKE b) LIKE c`), which is why this is a loop.
    fn parse_cmp(&mut self) -> Result<Item, ParseError> {
        let mut left = self.parse_add()?;
        loop {
            if self.is_kw("BETWEEN") {
                self.pos += 1;
                left = self.parse_between(left, false)?;
                continue;
            }
            if self.is_kw("NOT") {
                let save = self.pos;
                self.pos += 1;
                if self.is_kw("BETWEEN") {
                    self.pos += 1;
                    left = self.parse_between(left, true)?;
                    continue;
                }
                if self.is_kw("LIKE") {
                    self.pos += 1;
                    let right = self.parse_add()?;
                    left = Item::Binary(BinOp::NotLike, Box::new(left), Box::new(right));
                    continue;
                }
                // `exprMain NOT? IN (...)`
                if self.is_kw("IN") {
                    self.pos += 1;
                    left = self.parse_in(left, true)?;
                    continue;
                }
                self.pos = save;
            }
            if self.is_kw("LIKE") {
                self.pos += 1;
                let right = self.parse_add()?;
                left = Item::Binary(BinOp::Like, Box::new(left), Box::new(right));
                continue;
            }
            // `exprMain IS NOT? (TRUE|FALSE|NULL)`
            if self.is_kw("IS") {
                self.pos += 1;
                let negated = self.eat_kw("NOT");
                let kind = if self.eat_kw("NULL") {
                    IsKind::Null
                } else if self.eat_kw("TRUE") {
                    IsKind::True
                } else if self.eat_kw("FALSE") {
                    IsKind::False
                } else {
                    // `IS` must be followed by one of the three keywords: ANTLR reports the
                    // token it found there (`[a] IS 5` fails on `5`, `[a] IS` on the EOF).
                    return Err(self.error_here());
                };
                left = Item::Is {
                    value: Box::new(left),
                    kind,
                    negated,
                };
                continue;
            }
            // `exprMain NOT? IN (...)`
            if self.is_kw("IN") {
                self.pos += 1;
                left = self.parse_in(left, false)?;
                continue;
            }
            let op = match self.peek() {
                Tok::Eq => BinOp::Eq,
                Tok::Neq => BinOp::Neq,
                Tok::Lt => BinOp::Lt,
                Tok::Lte => BinOp::Lte,
                Tok::Gt => BinOp::Gt,
                Tok::Gte => BinOp::Gte,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.parse_add()?;
            left = Item::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn parse_between(&mut self, value: Item, negated: bool) -> Result<Item, ParseError> {
        let low = self.parse_add()?;
        self.expect_kw("AND")?;
        let high = self.parse_add()?;
        Ok(Item::Between {
            value: Box::new(value),
            low: Box::new(low),
            high: Box::new(high),
            negated,
        })
    }

    /// `exprMain [NOT] IN '(' (expression (',' expression)*)? ')'`
    ///
    /// The `IN` keyword is already consumed by the caller.
    fn parse_in(&mut self, value: Item, negated: bool) -> Result<Item, ParseError> {
        self.expect(&Tok::LParen)?;
        let mut list = Vec::new();
        if !matches!(self.peek(), Tok::RParen) {
            loop {
                list.push(self.parse_expr()?);
                if !matches!(self.peek(), Tok::Comma) {
                    break;
                }
                self.pos += 1;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(Item::In {
            value: Box::new(value),
            list,
            negated,
        })
    }

    fn parse_add(&mut self) -> Result<Item, ParseError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_mul()?;
            left = Item::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_mul(&mut self) -> Result<Item, ParseError> {
        let mut left = self.parse_pow()?;
        loop {
            let op = match self.peek() {
                Tok::Mul => BinOp::Mul,
                Tok::Div => BinOp::Div,
                Tok::Mod => BinOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_pow()?;
            left = Item::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_pow(&mut self) -> Result<Item, ParseError> {
        let left = self.parse_unary()?;
        if self.eat(&Tok::Caret) {
            let right = self.parse_pow()?;
            return Ok(Item::Binary(BinOp::Pow, Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Item, ParseError> {
        match self.peek() {
            // `exprMain: MINUS exprMain` — the grammar has no unary plus, so `+ 2` is an
            // `unexpected token` for ANTLR and must be rejected here as well.
            Tok::Minus => {
                self.pos += 1;
                Ok(Item::Unary(UnOp::Neg, Box::new(self.parse_unary()?)))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Item, ParseError> {
        let idx = self.pos;
        match self.next() {
            Tok::Field(name) => Ok(Item::Field(name)),
            Tok::Number { text, is_int } => Ok(Item::Num { text, is_int }),
            Tok::Str(text) => Ok(Item::Str(text)),
            Tok::Hash(hashes) => self.parse_date_literal(hashes, idx),
            Tok::LParen => {
                let inner = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(inner)
            }
            Tok::Ident(name) => self.parse_ident_item(&name),
            _ => Err(self.error_at(idx)),
        }
    }

    /// `dateLiteral: '#' DATE_INNER '#'` and its `##...##` and datetime variants.
    ///
    /// The three parts are separate tokens, so a malformed literal fails on the first part
    /// that does not fit, exactly like ANTLR (`#2020-01-01X#` reports `X`, `#2020-01-01##`
    /// reports `##`). The value itself is checked the way `dl_formula`'s visitor does after
    /// that: `datetime.date.fromisoformat()` for a date, `make_datetime_value()` for a
    /// datetime, both wrapped into `ParseDateValueError`/`ParseDatetimeValueError`.
    fn parse_date_literal(&mut self, hashes: u8, open_idx: usize) -> Result<Item, ParseError> {
        let (text, is_datetime) = match self.peek() {
            Tok::DateInner { text, is_datetime } => (text.clone(), *is_datetime),
            _ => return Err(self.error_here()),
        };
        self.pos += 1;
        if !self.eat(&Tok::Hash(hashes)) {
            return Err(self.error_here());
        }
        let span = Span {
            start: self.lexed.spans[open_idx].start,
            end: self.lexed.spans[self.pos - 1].end,
        };
        let valid = if is_datetime {
            is_valid_datetime(&text)
        } else {
            is_valid_date(&text)
        };
        if !valid {
            return Err(ParseError::invalid_date_value(&text, is_datetime, span));
        }
        Ok(Item::Date { text, is_datetime })
    }

    fn parse_ident_item(&mut self, name: &str) -> Result<Item, ParseError> {
        match name {
            "TRUE" => Ok(Item::Bool(true)),
            "FALSE" => Ok(Item::Bool(false)),
            "NULL" => Ok(Item::Null),
            // `IF(...)` / `CASE(...)` are the function forms of the blocks (`FuncIf` and
            // `FuncCase` in `dl_formula`); `IF cond THEN ... END` stays a block. The keyword is
            // case-insensitive, and both spellings of the call are the same construct.
            "IF" | "if" => self.parse_if_form(),
            "CASE" | "case" => self.parse_case_form(),
            // `IIF` is only the 3-argument legacy spelling of `IF`.
            "IIF" | "iif" => self.parse_conditional_function(name),
            // The keywords `DataLens.g4` does not list in `function` cannot start an
            // expression at all, so ANTLR fails on the keyword itself (`CASE WHEN ...`
            // reports `WHEN`). Every other name is a function call.
            "AND" | "OR" | "NOT" | "LIKE" | "BETWEEN" => self.parse_call(name),
            _ if is_reserved_keyword(name) => Err(self.error_at(self.pos - 1)),
            _ => self.parse_call(name),
        }
    }

    /// `IF cond THEN ... END` or `IF(cond, res, ..., else)`.
    ///
    /// The `(` after `IF` does not decide on its own: `IF (a - b) / 2 < 0 THEN` starts the
    /// condition with a parenthesized group, while `IF(a, b, c) / 2` is the call. ANTLR tries
    /// both alternatives and keeps the one that parses, so a failed call is retried as a block.
    fn parse_if_form(&mut self) -> Result<Item, ParseError> {
        if self.keyword_starts_function() {
            let save = self.pos;
            match self.parse_conditional_function("IF") {
                Ok(item) => return Ok(item),
                Err(error) => {
                    self.pos = save;
                    return self.parse_if().map_err(|_| error);
                }
            }
        }
        self.parse_if()
    }

    /// `CASE value WHEN ... END` or `CASE(value, when_1, res_1, ..., else)`; same two forms as
    /// [`Parser::parse_if_form`].
    fn parse_case_form(&mut self) -> Result<Item, ParseError> {
        if self.keyword_starts_function() {
            let save = self.pos;
            match self.parse_conditional_function("CASE") {
                Ok(item) => return Ok(item),
                Err(error) => {
                    self.pos = save;
                    return self.parse_case().map_err(|_| error);
                }
            }
        }
        self.parse_case()
    }

    /// `IF`/`CASE` open a block unless the parenthesized list after the keyword is the whole
    /// construct: `DataLens.g4` has both forms in `exprBasic`, and ANTLR picks the one that
    /// fits the tokens after the closing paren (`IF (a) THEN 1 END` is a block again).
    fn keyword_starts_function(&self) -> bool {
        if !matches!(self.peek(), Tok::LParen) {
            return false;
        }
        let mut depth = 0usize;
        for idx in self.pos..self.lexed.toks.len() {
            match self.lexed.toks[idx] {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        return !matches!(
                            self.lexed.toks.get(idx + 1),
                            Some(Tok::Ident(word))
                                if word.eq_ignore_ascii_case("THEN")
                                    || word.eq_ignore_ascii_case("WHEN")
                        );
                    }
                }
                Tok::Eof => return false,
                _ => {}
            }
        }
        false
    }

    /// `IF(cond_1, res_1, ..., else)` / `IIF(cond, res, else)` / `CASE(value, when_1, res_1,
    /// ..., [else])` lowered to a `Case` item.
    ///
    /// `dl_formula` maps these functions onto `_if_block_` and `_case_block_`
    /// (`FuncIfBase.variants` / `FuncCase.variants`), i.e. onto a SQL `CASE`, so the function
    /// spelling and the block spelling produce the same query.
    fn parse_conditional_function(&mut self, name: &str) -> Result<Item, ParseError> {
        let item = self.parse_call(name)?;
        let Item::Call { args, .. } = item else {
            return Err(self.error_here());
        };
        let mut args = args.into_iter();
        let operand = if name.eq_ignore_ascii_case("CASE") {
            // `CaseBlock.translation`: the first argument is the CASE operand.
            let Some(operand) = args.next() else {
                return Err(self.error_here());
            };
            Some(Box::new(operand))
        } else {
            None
        };
        let mut rest: Vec<Item> = args.collect();
        // An odd trailing argument is the `ELSE` (`CASE`) / the default result (`IF`).
        let else_ = if rest.len() % 2 == 1 {
            rest.pop().map(Box::new)
        } else {
            None
        };
        if rest.is_empty() {
            return Err(self.error_here());
        }
        let branches: Vec<(Item, Item)> = rest
            .chunks_exact(2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();
        Ok(Item::Case {
            operand,
            branches,
            else_,
        })
    }

    /// `IF cond THEN res [ELSEIF cond THEN res]* [ELSE res] END`
    /// lowered to a searched CASE expression.
    fn parse_if(&mut self) -> Result<Item, ParseError> {
        let mut branches = Vec::new();
        let cond = self.parse_expr()?;
        self.expect_kw("THEN")?;
        let res = self.parse_expr()?;
        branches.push((cond, res));
        while self.eat_kw("ELSEIF") {
            let cond = self.parse_expr()?;
            self.expect_kw("THEN")?;
            let res = self.parse_expr()?;
            branches.push((cond, res));
        }
        let else_ = if self.eat_kw("ELSE") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_kw("END")?;
        Ok(Item::Case {
            operand: None,
            branches,
            else_,
        })
    }

    /// `CASE operand WHEN value THEN res ... [ELSE res] END`
    /// (simple CASE is lowered to a searched CASE with an equality test).
    ///
    /// `caseBlock : CASE expression whenPart+ elsePart? END` — the operand is required, so
    /// `CASE WHEN ...` is not a searched CASE: it fails on `WHEN`, exactly like ANTLR.
    fn parse_case(&mut self) -> Result<Item, ParseError> {
        let operand = Some(Box::new(self.parse_expr()?));
        let mut branches = Vec::new();
        while self.eat_kw("WHEN") {
            let when = self.parse_expr()?;
            self.expect_kw("THEN")?;
            let then = self.parse_expr()?;
            branches.push((when, then));
        }
        if branches.is_empty() {
            // `CASE END` / `CASE ELSE ... END`: ANTLR reports the token that appears where
            // `WHEN` was required, which is the one the cursor is on here.
            return Err(self.error_here());
        }
        let else_ = if self.eat_kw("ELSE") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_kw("END")?;
        Ok(Item::Case {
            operand,
            branches,
            else_,
        })
    }

    /// `NAME(args [clauses])`
    ///
    /// A clause is only accepted inside the argument list: `DataLens.g4` has them in the
    /// `function` rule, so `SUM([a]) WITHIN [b]` is a syntax error on `WITHIN`, while
    /// `RSUM(SUM([a]) WITHIN [b])` binds the clause to `RSUM`, whose paren is still open.
    fn parse_call(&mut self, name: &str) -> Result<Item, ParseError> {
        let mut args = Vec::new();
        let mut clauses = Box::new(Clauses::default());
        self.expect(&Tok::LParen)?;
        if !self.eat(&Tok::RParen) {
            loop {
                args.push(self.parse_expr()?);
                // The grammar puts every clause after the last argument and before the
                // closing paren, so a clause keyword here ends the argument list and
                // belongs to this call.
                if self.at_clause_start() {
                    self.parse_clauses(&mut clauses)?;
                    self.expect(&Tok::RParen)?;
                    break;
                }
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen)?;
                break;
            }
        }
        self.check_clauses(name, &clauses)?;
        Ok(Item::Call {
            name: name.into(),
            args,
            clauses,
        })
    }

    /// `dl_formula.parser.base.resolve_function_capabilities()` + the `*clause is not
    /// supported by ...*` checks of `dl_formula.parser.base`.
    ///
    /// `dl_formula` rejects a clause on a function that has no such clause by raising
    /// `ParseClauseError` while post-processing the tree, i.e. after a successful parse, so
    /// such a formula is *not* a syntax error: the native parser has to reject it as well,
    /// and with no position, because `dl_formula` has none.
    fn check_clauses(&self, name: &str, clauses: &Clauses) -> Result<(), ParseError> {
        if clauses.is_empty() {
            return Ok(());
        }
        let has_grouping = clauses.has_grouping();
        let is_window = func_props::is_window_call(name, has_grouping);
        let func_kind = if is_window { "window" } else { "non-window" };
        // Same order as `resolve_function_capabilities()`: the first unsupported clause wins.
        let unsupported = [
            (
                has_grouping,
                func_props::supports_grouping(name, is_window),
                "Grouping",
            ),
            (
                !clauses.ordering.is_empty(),
                func_props::supports_ordering(name, is_window),
                "ORDER BY",
            ),
            (
                clauses.lod.is_some(),
                func_props::supports_lod(name, is_window),
                "Level-of-detail",
            ),
            (
                clauses.has_ignore_dimensions,
                func_props::supports_ignore_dimensions(name, is_window),
                "IGNORE DIMENSIONS",
            ),
            (
                clauses.has_bfb,
                func_props::supports_bfb(name, is_window),
                "BEFORE FILTER BY",
            ),
        ];
        for (present, supported, clause_name) in unsupported {
            if present && !supported {
                return Err(ParseError::clause(format!(
                    "{clause_name} clause is not supported by {func_kind} function {}",
                    name.to_uppercase()
                )));
            }
        }
        Ok(())
    }

    /// Does the next token start one of the clauses that may follow an argument list?
    fn at_clause_start(&self) -> bool {
        match self.peek() {
            Tok::Ident(name) => match name.as_ref() {
                "TOTAL" | "AMONG" | "WITHIN" | "FIXED" | "INCLUDE" | "EXCLUDE" => true,
                // `ORDER BY`, `BEFORE FILTER BY` and `IGNORE DIMENSIONS` are single tokens of
                // the grammar, so their words only start a clause when they are all there.
                "ORDER" => self.phrase_at(&["ORDER", "BY"]),
                "BEFORE" => self.phrase_at(&["BEFORE", "FILTER", "BY"]),
                "IGNORE" => self.phrase_at(&["IGNORE", "DIMENSIONS"]),
                _ => false,
            },
            _ => false,
        }
    }

    fn is_kw_at(&self, idx: usize, kw: &str) -> bool {
        matches!(self.lexed.toks.get(idx), Some(Tok::Ident(name)) if name.as_ref() == kw)
    }

    /// Is there a run of spaces (and nothing else) between the two tokens?
    fn spaces_between(&self, left: usize, right: usize) -> bool {
        match (self.lexed.spans.get(left), self.lexed.spans.get(right)) {
            (Some(left), Some(right)) => {
                let gap = &self.text[left.end as usize..right.start as usize];
                !gap.is_empty() && gap.bytes().all(|b| b == b' ')
            }
            _ => false,
        }
    }

    /// Is the multi-word token `words` (`["ORDER", "BY"]`, ...) at the cursor?
    ///
    /// `DataLens.g4` spells `ORDER_BY`, `BEFORE_FILTER_BY` and `IGNORE_DIMENSIONS` as single
    /// tokens with `' '+` between the words, so a newline or a tab breaks them apart: then
    /// `ORDER` is just a function name and the clause does not exist (`SUM([a] ORDER [b])`
    /// fails on `ORDER`, and so does `SUM([a] ORDER\nBY [b])`).
    fn phrase_at(&self, words: &[&str]) -> bool {
        self.phrase_len_from(self.pos, words).is_some()
    }

    fn eat_phrase(&mut self, words: &[&str]) -> bool {
        if !self.phrase_at(words) {
            return false;
        }
        self.pos += words.len();
        true
    }

    /// `winGrouping? ordering? lodSpecifier? beforeFilterBy? ignoreDimensions?`
    ///
    /// The order is fixed by `DataLens.g4`, so the clauses are parsed one after another
    /// rather than in a loop. `dl_formula` validates which function accepts which clause
    /// (`func_props::clause_flags`); the parser only builds the tree.
    fn parse_clauses(&mut self, clauses: &mut Clauses) -> Result<(), ParseError> {
        if self.eat_kw("TOTAL") {
            clauses.grouping = Grouping::Total;
        } else if self.eat_kw("AMONG") {
            clauses.grouping = Grouping::Among;
            self.parse_dim_list(&mut clauses.grouping_dims)?;
        } else if self.eat_kw("WITHIN") {
            clauses.grouping = Grouping::Within;
            self.parse_dim_list(&mut clauses.grouping_dims)?;
        }

        if self.eat_phrase(&["ORDER", "BY"]) {
            loop {
                let item = self.parse_expr()?;
                let direction = if self.eat_kw("DESC") {
                    OrderingDirection::Desc
                } else {
                    self.eat_kw("ASC");
                    OrderingDirection::Asc
                };
                clauses.ordering.push((item, direction));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }

        let lod_kind = if self.eat_kw("FIXED") {
            Some(LodKind::Fixed)
        } else if self.eat_kw("INCLUDE") {
            Some(LodKind::Include)
        } else if self.eat_kw("EXCLUDE") {
            Some(LodKind::Exclude)
        } else {
            None
        };
        if let Some(kind) = lod_kind {
            let mut dims = Vec::new();
            self.parse_dim_list(&mut dims)?;
            clauses.lod = Some(Lod { kind, dims });
        }

        // In the grammar `BEFORE FILTER BY` is a single token with `' '+` between the words,
        // so any amount of spaces is allowed but nothing else is; the lexer splits it into
        // three identifiers.
        if self.eat_phrase(&["BEFORE", "FILTER", "BY"]) {
            clauses.has_bfb = true;
            loop {
                let field = match self.peek() {
                    Tok::Field(name) => name.clone(),
                    _ => break,
                };
                self.pos += 1;
                clauses.bfb.push(field);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }

        if self.eat_phrase(&["IGNORE", "DIMENSIONS"]) {
            clauses.has_ignore_dimensions = true;
            self.parse_dim_list(&mut clauses.ignore_dimensions)?;
        }
        Ok(())
    }

    /// A comma-separated list of dimensions. Every clause list in the grammar is optional,
    /// so an empty list is valid (`SUM([a] FIXED)`, `BEFORE FILTER BY` with no names).
    fn parse_dim_list(&mut self, dims: &mut Vec<Item>) -> Result<(), ParseError> {
        loop {
            if matches!(self.peek(), Tok::RParen | Tok::Eof) || self.at_clause_word() {
                return Ok(());
            }
            dims.push(self.parse_expr()?);
            if !self.eat(&Tok::Comma) {
                return Ok(());
            }
        }
    }

    /// Is the cursor on a word that can only start a clause, even when that clause is broken?
    ///
    /// `SUM([a] FIXED [b] ORDER [c])` ends the dimension list before the broken `ORDER BY`, so
    /// the caller reports `ORDER` — the token ANTLR trips over.
    fn at_clause_word(&self) -> bool {
        matches!(self.peek(), Tok::Ident(name) if matches!(
            name.as_ref(),
            "TOTAL" | "AMONG" | "WITHIN" | "ORDER" | "FIXED" | "INCLUDE" | "EXCLUDE" | "BEFORE" | "IGNORE"
        ))
    }
}

/// `dl_formula.parser.antlr.visitor.visitDateLiteral()`: the text between the hashes is passed
/// to `datetime.date.fromisoformat()`, and a `ValueError` becomes `ParseDateValueError`.
///
/// The lexer guarantees the `DDDD-DD-DD` shape, so only the calendar is left to check.
fn is_valid_date(text: &str) -> bool {
    let Some((year, rest)) = text.split_at_checked(4) else {
        return false;
    };
    let (Some(year), Some(month), Some(day)) = (
        year.parse::<u32>().ok(),
        rest.get(1..3).and_then(|part| part.parse::<u32>().ok()),
        rest.get(4..6).and_then(|part| part.parse::<u32>().ok()),
    ) else {
        return false;
    };
    year >= 1 && (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)
}

/// `dl_formula.utils.datetime.parse_dt_string()`: the value is formatted into one of
/// `%Y-%m-%d`, `%Y-%m-%d %H`, `%Y-%m-%d %H:%M` and `%Y-%m-%d %H:%M:%S` (the separator may be a
/// space or `T`), and `strptime` rejects impossible dates and times — so `#2020-01-01 25#` and
/// `#2020-01-01 10:00:60#` are `ParseDatetimeValueError`s.
fn is_valid_datetime(text: &str) -> bool {
    let Some((date, rest)) = text.split_at_checked(10) else {
        return false;
    };
    if !is_valid_date(date) {
        return false;
    }
    // The separator is a single character (`T`, `t` or a space); without a time part
    // (`#2019-01-01T#`) the value is midnight, exactly as `strptime` pads it.
    let time = rest.get(1..).unwrap_or("");
    if time.is_empty() {
        return true;
    }
    let fields: Vec<&str> = time.split(':').collect();
    fields.len() <= 3
        && fields
            .iter()
            .zip([23, 59, 59])
            .all(|(field, limit)| field.parse::<u32>().is_ok_and(|value| value <= limit))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
    }
}

/// Keywords that `DataLens.g4` defines as lexer tokens but does not allow at the start of an
/// expression (`function` accepts only `FUNC_NAME|CASE|IF|NOT|AND|OR|LIKE|BETWEEN`).
///
/// ANTLR therefore fails on such a keyword instead of trying to call it as a function, so
/// `CASE WHEN ...` reports `WHEN` and `CASE END` reports `END`. The words of the multi-word
/// tokens (`ORDER`, `BY`, `BEFORE`, `FILTER`, `IGNORE`, `DIMENSIONS`) are *not* keywords of
/// their own — the grammar lexes them as `FUNC_NAME` outside a complete clause.
fn is_reserved_keyword(name: &str) -> bool {
    matches!(
        name,
        "AMONG"
            | "ASC"
            | "DESC"
            | "ELSE"
            | "ELSEIF"
            | "END"
            | "EXCLUDE"
            | "FIXED"
            | "IN"
            | "INCLUDE"
            | "IS"
            | "THEN"
            | "TOTAL"
            | "WHEN"
            | "WITHIN"
    )
}
