//! Issue #115: a pure arithmetic expression evaluator, no model involved.
//!
//! Recursive descent over a hand-rolled tokenizer -- no `eval`, no shelling
//! out, nothing but `f64` arithmetic. Grammar (loosest to tightest binding):
//!
//! ```text
//! expr   := term (('+' | '-') term)*
//! term   := unary (('*' | '/' | '%') unary)*
//! unary  := ('-' | '+') unary | power
//! power  := primary ('^' unary)?          // right-associative
//! primary:= number | ident ['(' expr ')'] | '(' expr ')'
//! ```
//!
//! `unary` binds looser than `^` (so `-2^2` is `-(2^2)` = `-4`, matching
//! Python and most calculators) but tighter than `*`/`/` (so `-2*3` is
//! `(-2)*3`, not `-(2*3)` -- same result here, but the grammar shape is what
//! makes that a fact rather than a coincidence).
//!
//! Both nesting depth (parens, function-call arguments, and chained unary
//! signs) and total input length are bounded (see [`MAX_DEPTH`] /
//! [`super::MAX_INPUT_LEN`]) so a hostile or accidental `((((((((...` or
//! `------...` input cannot blow the stack -- this crate's release profile
//! is `panic = "abort"` (AGENTS.md's stack table), so a stack overflow here
//! would kill the whole tray app, not just this one calculation.

use std::fmt;

/// Recursion-depth ceiling for parens, function-call arguments, and chained
/// unary signs. 64 is generously more than any selected text a user would
/// plausibly paste ("(((1)))" is depth 3), while still small enough that a
/// deliberately pathological `(` * 10000 input fails fast and cleanly
/// instead of recursing anywhere near the real stack limit.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum ExprError {
    Empty,
    TooLong,
    TooDeep,
    UnexpectedChar(char, usize),
    InvalidNumber(String),
    UnexpectedEnd,
    UnexpectedToken(String),
    UnknownFunction(String),
    UnknownConstant(String),
    DivisionByZero,
    /// A binary or unary operation produced +/-infinity (e.g. `10^400`).
    Overflow,
    /// A binary or unary operation produced NaN (e.g. `sqrt(-1)` reached
    /// through `^` with a fractional exponent, rather than through the
    /// `sqrt` function itself, which reports [`ExprError::DomainError`]
    /// instead with a clearer message).
    NotReal,
    DomainError(String),
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExprError::Empty => write!(f, "Nothing to calculate."),
            ExprError::TooLong => write!(f, "That expression is too long."),
            ExprError::TooDeep => write!(f, "That expression is nested too deeply."),
            ExprError::UnexpectedChar(c, pos) => {
                write!(f, "Unexpected character '{c}' at position {pos}.")
            }
            ExprError::InvalidNumber(s) => write!(f, "'{s}' is not a valid number."),
            ExprError::UnexpectedEnd => write!(f, "The expression ends unexpectedly."),
            ExprError::UnexpectedToken(t) => write!(f, "Unexpected {t} in expression."),
            ExprError::UnknownFunction(name) => write!(f, "Unknown function '{name}'."),
            ExprError::UnknownConstant(name) => write!(f, "Unknown constant '{name}'."),
            ExprError::DivisionByZero => write!(f, "Division by zero."),
            ExprError::Overflow => write!(f, "The result is too large to represent."),
            ExprError::NotReal => write!(f, "That has no real result."),
            ExprError::DomainError(msg) => write!(f, "{msg}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    Percent,
    LParen,
    RParen,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Number(n) => write!(f, "number {n}"),
            Token::Ident(s) => write!(f, "identifier '{s}'"),
            Token::Plus => write!(f, "'+'"),
            Token::Minus => write!(f, "'-'"),
            Token::Star => write!(f, "'*'"),
            Token::Slash => write!(f, "'/'"),
            Token::Caret => write!(f, "'^'"),
            Token::Percent => write!(f, "'%'"),
            Token::LParen => write!(f, "'('"),
            Token::RParen => write!(f, "')'"),
        }
    }
}

fn tokenize(input: &str) -> Result<Vec<Token>, ExprError> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut tokens = Vec::new();

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '+' => {
                tokens.push(Token::Plus);
                i += 1;
            }
            '-' => {
                tokens.push(Token::Minus);
                i += 1;
            }
            '*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            '/' => {
                tokens.push(Token::Slash);
                i += 1;
            }
            '^' => {
                tokens.push(Token::Caret);
                i += 1;
            }
            '%' => {
                tokens.push(Token::Percent);
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            d if d.is_ascii_digit() || d == '.' => {
                let start = i;
                let mut seen_dot = d == '.';
                i += 1;
                loop {
                    if i >= chars.len() {
                        break;
                    }
                    let d = chars[i];
                    if d.is_ascii_digit() {
                        i += 1;
                    } else if d == ',' {
                        // Thousands separator: only consumed when sandwiched
                        // between digits, so "1,234" reads as one number but
                        // a trailing "1," does not eat the comma.
                        if chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()) {
                            i += 1;
                        } else {
                            break;
                        }
                    } else if d == '.' && !seen_dot {
                        seen_dot = true;
                        i += 1;
                    } else if d == 'e' || d == 'E' {
                        let mut j = i + 1;
                        if matches!(chars.get(j), Some('+') | Some('-')) {
                            j += 1;
                        }
                        if chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
                            i = j;
                            while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                                i += 1;
                            }
                        }
                        // Whether or not a real exponent was found, the
                        // number literal ends here -- a lone trailing 'e'
                        // with no digits after it is left for the next
                        // token (see the module doc's number-lexing note).
                        break;
                    } else {
                        break;
                    }
                }
                let raw: String = chars[start..i].iter().collect();
                let cleaned: String = raw.chars().filter(|&c| c != ',').collect();
                let value: f64 = cleaned
                    .parse()
                    .map_err(|_| ExprError::InvalidNumber(raw.clone()))?;
                tokens.push(Token::Number(value));
            }
            a if a.is_alphabetic() || a == '_' => {
                let start = i;
                while chars
                    .get(i)
                    .is_some_and(|c| c.is_alphanumeric() || *c == '_')
                {
                    i += 1;
                }
                let ident: String = chars[start..i].iter().collect();
                tokens.push(Token::Ident(ident));
            }
            other => return Err(ExprError::UnexpectedChar(other, i)),
        }
    }

    Ok(tokens)
}

fn apply_function(name: &str, arg: f64) -> Result<f64, ExprError> {
    match name.to_ascii_lowercase().as_str() {
        "sqrt" => {
            if arg < 0.0 {
                Err(ExprError::DomainError(
                    "Can't take the square root of a negative number.".to_string(),
                ))
            } else {
                Ok(arg.sqrt())
            }
        }
        "sin" => Ok(arg.sin()),
        "cos" => Ok(arg.cos()),
        "tan" => Ok(arg.tan()),
        "ln" => {
            if arg <= 0.0 {
                Err(ExprError::DomainError(
                    "ln is only defined for positive numbers.".to_string(),
                ))
            } else {
                Ok(arg.ln())
            }
        }
        "log" => {
            if arg <= 0.0 {
                Err(ExprError::DomainError(
                    "log is only defined for positive numbers.".to_string(),
                ))
            } else {
                Ok(arg.log10())
            }
        }
        "abs" => Ok(arg.abs()),
        other => Err(ExprError::UnknownFunction(other.to_string())),
    }
}

fn constant(name: &str) -> Result<f64, ExprError> {
    match name.to_ascii_lowercase().as_str() {
        "pi" => Ok(std::f64::consts::PI),
        "e" => Ok(std::f64::consts::E),
        other => Err(ExprError::UnknownConstant(other.to_string())),
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: Token) -> Result<(), ExprError> {
        match self.peek() {
            Some(t) if *t == expected => {
                self.advance();
                Ok(())
            }
            Some(t) => Err(ExprError::UnexpectedToken(format!(
                "{t} (expected {expected})"
            ))),
            None => Err(ExprError::UnexpectedEnd),
        }
    }

    /// Enters one level of nesting (a paren, a function-call argument, or
    /// one link of a unary-sign chain). Returning `Err` leaves `depth`
    /// incremented, but that is harmless: an error here aborts the whole
    /// parse immediately (see [`evaluate`]), so nothing ever reads `depth`
    /// again. On the success path, every caller pairs this with
    /// [`Parser::exit_depth`] once the nested parse returns, whether it
    /// succeeded or failed, so sibling subtrees (e.g. `1+2+3+...`, which
    /// calls `term()` repeatedly but never nests) are never miscounted as
    /// deep.
    fn enter_depth(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ExprError::TooDeep);
        }
        Ok(())
    }

    fn exit_depth(&mut self) {
        self.depth -= 1;
    }

    fn expr(&mut self) -> Result<f64, ExprError> {
        let mut value = self.term()?;
        loop {
            match self.peek() {
                Some(Token::Plus) => {
                    self.advance();
                    value += self.term()?;
                }
                Some(Token::Minus) => {
                    self.advance();
                    value -= self.term()?;
                }
                _ => break,
            }
            check_finite(value)?;
        }
        Ok(value)
    }

    fn term(&mut self) -> Result<f64, ExprError> {
        let mut value = self.unary()?;
        loop {
            match self.peek() {
                Some(Token::Star) => {
                    self.advance();
                    value *= self.unary()?;
                }
                Some(Token::Slash) => {
                    self.advance();
                    let rhs = self.unary()?;
                    if rhs == 0.0 {
                        return Err(ExprError::DivisionByZero);
                    }
                    value /= rhs;
                }
                Some(Token::Percent) => {
                    self.advance();
                    let rhs = self.unary()?;
                    if rhs == 0.0 {
                        return Err(ExprError::DivisionByZero);
                    }
                    value %= rhs;
                }
                _ => break,
            }
            check_finite(value)?;
        }
        Ok(value)
    }

    fn unary(&mut self) -> Result<f64, ExprError> {
        match self.peek() {
            Some(Token::Minus) => {
                self.advance();
                self.enter_depth()?;
                let r = self.unary();
                self.exit_depth();
                Ok(-(r?))
            }
            Some(Token::Plus) => {
                self.advance();
                self.enter_depth()?;
                let r = self.unary();
                self.exit_depth();
                r
            }
            _ => self.power(),
        }
    }

    fn power(&mut self) -> Result<f64, ExprError> {
        let base = self.primary()?;
        if let Some(Token::Caret) = self.peek() {
            self.advance();
            self.enter_depth()?;
            let exponent = self.unary();
            self.exit_depth();
            let exponent = exponent?;
            let value = base.powf(exponent);
            if value.is_nan() {
                return Err(ExprError::NotReal);
            }
            if value.is_infinite() {
                return Err(ExprError::Overflow);
            }
            return Ok(value);
        }
        Ok(base)
    }

    fn primary(&mut self) -> Result<f64, ExprError> {
        match self.peek().cloned() {
            Some(Token::Number(n)) => {
                self.advance();
                Ok(n)
            }
            Some(Token::LParen) => {
                self.advance();
                self.enter_depth()?;
                let inner = self.expr();
                self.exit_depth();
                let value = inner?;
                self.expect(Token::RParen)?;
                Ok(value)
            }
            Some(Token::Ident(name)) => {
                self.advance();
                if matches!(self.peek(), Some(Token::LParen)) {
                    self.advance();
                    self.enter_depth()?;
                    let arg = self.expr();
                    self.exit_depth();
                    let arg = arg?;
                    self.expect(Token::RParen)?;
                    apply_function(&name, arg)
                } else {
                    constant(&name)
                }
            }
            Some(other) => Err(ExprError::UnexpectedToken(other.to_string())),
            None => Err(ExprError::UnexpectedEnd),
        }
    }
}

fn check_finite(value: f64) -> Result<(), ExprError> {
    if value.is_nan() {
        return Err(ExprError::NotReal);
    }
    if value.is_infinite() {
        return Err(ExprError::Overflow);
    }
    Ok(())
}

/// Evaluates a bounded-length arithmetic expression to an `f64`. Never
/// panics on malformed input -- every failure comes back as a named
/// [`ExprError`] (AGENTS.md rule 7's "every failure ends in a card" starts
/// here: this is the pure layer a card's text is built from).
pub fn evaluate(input: &str) -> Result<f64, ExprError> {
    if input.trim().is_empty() {
        return Err(ExprError::Empty);
    }
    if input.len() > super::MAX_INPUT_LEN {
        return Err(ExprError::TooLong);
    }
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(ExprError::Empty);
    }
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        depth: 0,
    };
    let value = parser.expr()?;
    if parser.pos != tokens.len() {
        return Err(ExprError::UnexpectedToken(tokens[parser.pos].to_string()));
    }
    check_finite(value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- basic arithmetic, precedence, associativity ------------------------

    #[test]
    fn table_of_valid_expressions() {
        let cases: &[(&str, f64)] = &[
            ("1+1", 2.0),
            ("2 - 3", -1.0),
            ("2*3", 6.0),
            ("(12*7)/4", 21.0),
            ("10/4", 2.5),
            ("2+3*4", 14.0),
            ("(2+3)*4", 20.0),
            ("2^3", 8.0),
            ("2^3^2", 512.0), // right-assoc: 2^(3^2) = 2^9
            ("-2^2", -4.0),   // unary binds looser than ^
            ("(-2)^2", 4.0),
            ("-5", -5.0),
            ("--5", 5.0),
            ("+-5", -5.0),
            ("10%3", 1.0),
            ("2.5*2", 5.0),
            ("1,234+1", 1235.0),
            ("1,234.5+0.5", 1235.0),
            ("1.5e3", 1500.0),
            ("2E-2", 0.02),
            ("sqrt(16)", 4.0),
            ("abs(-5)", 5.0),
            ("ln(1)", 0.0),
            ("log(100)", 2.0),
            ("sin(0)", 0.0),
            ("cos(0)", 1.0),
            ("pi", std::f64::consts::PI),
            ("e", std::f64::consts::E),
            ("2*pi", std::f64::consts::PI * 2.0),
            ("((((1))))", 1.0),
            ("1 + 2 * (3 - 1)", 5.0),
            ("100 % 9", 1.0),
            ("0-0", 0.0),
            ("0.1+0.2", 0.30000000000000004),
        ];
        for (input, expected) in cases {
            let got = evaluate(input).unwrap_or_else(|e| panic!("{input} failed: {e}"));
            assert!(
                (got - expected).abs() < 1e-9 * expected.abs().max(1.0),
                "{input}: expected {expected}, got {got}"
            );
        }
    }

    // -- error cases ----------------------------------------------------------

    #[test]
    fn table_of_error_cases() {
        type Matcher = fn(&ExprError) -> bool;
        let cases: &[(&str, Matcher)] = &[
            ("", |e| matches!(e, ExprError::Empty)),
            ("   ", |e| matches!(e, ExprError::Empty)),
            ("1/0", |e| matches!(e, ExprError::DivisionByZero)),
            ("1%0", |e| matches!(e, ExprError::DivisionByZero)),
            ("0/0", |e| matches!(e, ExprError::DivisionByZero)),
            ("1+", |e| matches!(e, ExprError::UnexpectedEnd)),
            ("(1+2", |e| matches!(e, ExprError::UnexpectedEnd)),
            ("1+2)", |e| matches!(e, ExprError::UnexpectedToken(_))),
            ("1 2", |e| matches!(e, ExprError::UnexpectedToken(_))),
            ("sqrt(-1)", |e| matches!(e, ExprError::DomainError(_))),
            ("ln(-1)", |e| matches!(e, ExprError::DomainError(_))),
            ("ln(0)", |e| matches!(e, ExprError::DomainError(_))),
            ("log(0)", |e| matches!(e, ExprError::DomainError(_))),
            ("10^400", |e| matches!(e, ExprError::Overflow)),
            ("-10^400", |e| matches!(e, ExprError::Overflow)),
            ("bananas(1)", |e| matches!(e, ExprError::UnknownFunction(_))),
            ("bananas", |e| matches!(e, ExprError::UnknownConstant(_))),
            ("1@2", |e| matches!(e, ExprError::UnexpectedChar('@', 1))),
            ("()", |e| matches!(e, ExprError::UnexpectedToken(_))),
        ];
        for (input, matcher) in cases {
            let err = evaluate(input).expect_err(&format!("{input} should have failed"));
            assert!(matcher(&err), "{input}: wrong error variant: {err:?}");
        }
    }

    #[test]
    fn deep_nesting_is_rejected_not_a_stack_overflow() {
        let deep_parens = "(".repeat(200) + "1" + &")".repeat(200);
        assert!(matches!(evaluate(&deep_parens), Err(ExprError::TooDeep)));

        let deep_unary = "-".repeat(200) + "1";
        assert!(matches!(evaluate(&deep_unary), Err(ExprError::TooDeep)));
    }

    #[test]
    fn moderate_nesting_within_the_limit_still_evaluates() {
        let ok_parens = "(".repeat(10) + "1" + &")".repeat(10);
        assert_eq!(evaluate(&ok_parens), Ok(1.0));
    }

    #[test]
    fn a_long_flat_chain_is_not_mistaken_for_deep_nesting() {
        // Sequential '+' operands at the SAME recursion depth, not nested --
        // must not trip MAX_DEPTH (well over it in operand count) just
        // because there are many of them. Kept under MAX_INPUT_LEN so this
        // test is purely about depth, not the separate length bound (see
        // `too_long_input_is_rejected` for that one).
        let count = MAX_DEPTH * 2;
        let chain = (0..count).map(|_| "1").collect::<Vec<_>>().join("+");
        assert!(chain.len() <= super::super::MAX_INPUT_LEN);
        assert_eq!(evaluate(&chain), Ok(count as f64));
    }

    #[test]
    fn too_long_input_is_rejected() {
        let long = "1+".repeat(super::super::MAX_INPUT_LEN);
        assert!(matches!(evaluate(&long), Err(ExprError::TooLong)));
    }

    #[test]
    fn unknown_char_reports_its_position() {
        let err = evaluate("1+$2").unwrap_err();
        assert_eq!(err, ExprError::UnexpectedChar('$', 2));
    }

    #[test]
    fn case_insensitive_function_and_constant_names() {
        assert_eq!(evaluate("SQRT(4)"), Ok(2.0));
        assert_eq!(evaluate("PI"), Ok(std::f64::consts::PI));
    }

    #[test]
    fn display_messages_have_no_em_dash() {
        // AGENTS.md rule 11: no em dashes in user-facing strings, and every
        // ExprError's Display text is exactly that.
        let samples = [
            ExprError::Empty,
            ExprError::TooLong,
            ExprError::TooDeep,
            ExprError::UnexpectedChar('$', 0),
            ExprError::InvalidNumber("1.2.3".to_string()),
            ExprError::UnexpectedEnd,
            ExprError::UnexpectedToken("')'".to_string()),
            ExprError::UnknownFunction("foo".to_string()),
            ExprError::UnknownConstant("foo".to_string()),
            ExprError::DivisionByZero,
            ExprError::Overflow,
            ExprError::NotReal,
            ExprError::DomainError("ln is only defined for positive numbers.".to_string()),
        ];
        for e in samples {
            assert!(!e.to_string().contains('\u{2014}'), "{e}");
        }
    }
}
