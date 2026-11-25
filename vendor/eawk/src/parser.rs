//! Recursive-descent parser for awk.
//!
//! Operator precedence follows the POSIX table, with two points that are easy
//! to get backwards and that were each verified against real awks:
//!
//! * Unary `-` binds **looser** than `^`, so `-2^2` is `-(2^2)` and evaluates
//!   to `-4`.
//! * `^` is right-associative: `2^3^2` is `2^(3^2)` = 512.
//!
//! Comparison operators are non-associative — `1<2<3` is a syntax error, not
//! `(1<2)<3`.

use crate::ast::*;
use crate::error::Error;
use crate::lexer::{Keyword, Lexer, Op, Spanned, Tok};
use std::collections::HashMap;
use std::rc::Rc;

/// Nesting limit for the recursive-descent parser.
///
/// A stack overflow cannot be caught by `Result`, so pathological input has to
/// be rejected before the stack runs out, not after. One level of expression
/// nesting costs roughly a dozen stack frames (expression → … → primary), and
/// `unary` counts separately, so a limit of 100 corresponds to about 50 nested
/// parentheses. Real programs nest a handful of levels deep at most; the value
/// was chosen by measuring where the overflow actually begins.
const MAX_NESTING: usize = 100;

/// Parses a complete awk program.
pub fn parse_program(src: &str) -> Result<Program, Error> {
    let toks = Lexer::new(src).tokenize()?;
    Parser::new(toks).parse_program()
}

/// Parses a single expression. Used by the embedding API and by tests.
pub fn parse_expression(src: &str) -> Result<Expr, Error> {
    let toks = Lexer::new(src).tokenize()?;
    let mut p = Parser::new(toks);
    let e = p.expression()?;
    p.skip_newlines();
    p.expect_eof()?;
    Ok(e)
}

struct Parser {
    toks: Vec<Spanned>,
    pos: usize,
    depth: usize,
    /// While greater than zero, a bare `>` is not a comparison operator.
    ///
    /// In awk a bare `>` after a `print` or `printf` argument is a redirection
    /// target, not a comparison: `print 1 > "f"` writes to a file. Inside
    /// parentheses the comparison is still available, so `print (1 > 0)`
    /// compares as you would expect.
    no_gt: u32,
}

impl Parser {
    fn new(toks: Vec<Spanned>) -> Parser {
        Parser { toks, pos: 0, depth: 0, no_gt: 0 }
    }

    // ---- token helpers -------------------------------------------------

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn line(&self) -> usize {
        self.toks[self.pos.min(self.toks.len() - 1)].line
    }

    fn column(&self) -> usize {
        self.toks[self.pos.min(self.toks.len() - 1)].column
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    /// Skip statement-terminating newlines. awk allows a statement to continue
    /// across lines after many tokens.
    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Tok::Newline) {
            self.pos += 1;
        }
    }

    fn at_op(&self, o: Op) -> bool {
        matches!(self.peek(), Tok::Op(x) if *x == o)
    }

    fn at_kw(&self, k: Keyword) -> bool {
        matches!(self.peek(), Tok::Kw(x) if *x == k)
    }

    fn eat_op(&mut self, o: Op) -> bool {
        if self.at_op(o) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: Keyword) -> bool {
        if self.at_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, o: Op) -> Result<(), Error> {
        if self.eat_op(o) {
            Ok(())
        } else {
            Err(self.error(format!("expected `{}`, found {}", crate::lexer::op_str(o), self.peek().describe())))
        }
    }

    fn expect_kw(&mut self, k: Keyword) -> Result<(), Error> {
        if self.eat_kw(k) {
            Ok(())
        } else {
            Err(self.error(format!("expected `{}`, found {}", crate::lexer::keyword_str(k), self.peek().describe())))
        }
    }

    fn expect_ident(&mut self) -> Result<Rc<str>, Error> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.bump();
                Ok(s)
            }
            other => Err(self.error(format!("expected a name, found {}", other.describe()))),
        }
    }

    fn expect_eof(&mut self) -> Result<(), Error> {
        self.skip_newlines();
        if matches!(self.peek(), Tok::Eof) {
            Ok(())
        } else {
            Err(self.error(format!("unexpected {} at top level", self.peek().describe())))
        }
    }

    fn error(&self, msg: impl Into<String>) -> Error {
        Error::parse(msg).at(self.line(), self.column())
    }

    /// Track recursion depth so deeply nested input errors instead of
    /// overflowing the stack.
    fn enter(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return Err(self.error("expression nested too deeply"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    // ---- program -------------------------------------------------------

    fn parse_program(mut self) -> Result<Program, Error> {
        let mut prog = Program::default();
        let mut funcs: HashMap<Rc<str>, Rc<FuncDef>> = HashMap::new();
        self.skip_newlines();
        while !matches!(self.peek(), Tok::Eof) {
            let item = self.parse_item(&mut funcs)?;
            prog.items.push(item);
            self.skip_newlines();
        }
        // Resolve function calls only after every definition is seen, so a
        // function may be used before it is defined.
        prog.funcs = funcs;
        Ok(prog)
    }

    fn parse_item(&mut self, funcs: &mut HashMap<Rc<str>, Rc<FuncDef>>) -> Result<Item, Error> {
        if self.at_kw(Keyword::Begin) {
            self.bump();
            let body = self.parse_block()?;
            return Ok(Item::Begin(body));
        }
        if self.at_kw(Keyword::End) {
            self.bump();
            let body = self.parse_block()?;
            return Ok(Item::End(body));
        }
        if self.at_kw(Keyword::Function) {
            let fd = self.parse_function(funcs)?;
            return Ok(Item::Func(Rc::new(fd)));
        }
        Ok(Item::Rule { pattern: self.parse_pattern()?, action: self.parse_optional_action()? })
    }

    /// A rule's left-hand side. A leading `/re/` or an unadorned expression is
    /// a pattern; `{ ... }` is an always-run action.
    fn parse_pattern(&mut self) -> Result<Pattern, Error> {
        if self.at_op(Op::LBrace) {
            return Ok(Pattern::Always);
        }
        let first = self.expression()?;
        self.skip_newlines();
        if self.at_op(Op::Comma) {
            self.bump();
            self.skip_newlines();
            let second = self.expression()?;
            return Ok(Pattern::Range(Rc::new(first), Rc::new(second)));
        }
        Ok(Pattern::Expr(Rc::new(first)))
    }

    fn parse_optional_action(&mut self) -> Result<Option<Rc<Block>>, Error> {
        self.skip_newlines();
        if self.at_op(Op::LBrace) {
            Ok(Some(self.parse_block()?))
        } else {
            Ok(None)
        }
    }

    fn parse_function(&mut self, funcs: &mut HashMap<Rc<str>, Rc<FuncDef>>) -> Result<FuncDef, Error> {
        self.expect_kw(Keyword::Function)?;
        self.skip_newlines();
        let name = self.expect_ident()?;
        self.skip_newlines();
        // An optional parenthesized parameter list; POSIX requires the parens
        // but real code omits them often enough to accept.
        let mut params = Vec::new();
        if self.eat_op(Op::LParen) {
            self.skip_newlines();
            if !self.at_op(Op::RParen) {
                loop {
                    self.skip_newlines();
                    params.push(self.expect_ident()?);
                    self.skip_newlines();
                    if !self.eat_op(Op::Comma) {
                        break;
                    }
                }
            }
            self.skip_newlines();
            self.expect_op(Op::RParen)?;
        }
        self.skip_newlines();
        let body = self.parse_block()?;
        let fd = FuncDef { name: name.clone(), params, body };
        if let Some(prev) = funcs.insert(name.clone(), Rc::new(fd.clone())) {
            let _ = prev;
        }
        Ok(fd)
    }

    // ---- statements ----------------------------------------------------

    fn parse_block(&mut self) -> Result<Rc<Block>, Error> {
        self.enter()?;
        let result = self.parse_block_inner();
        self.leave();
        result
    }

    fn parse_block_inner(&mut self) -> Result<Rc<Block>, Error> {
        self.expect_op(Op::LBrace)?;
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines();
            while self.eat_op(Op::Semicolon) {
                self.skip_newlines();
            }
            if self.at_op(Op::RBrace) {
                self.bump();
                break;
            }
            if matches!(self.peek(), Tok::Eof) {
                return Err(self.error("unexpected end of input; expected `}`"));
            }
            stmts.push(self.parse_stmt()?);
            // A statement ends at a newline or `;`; `}` closes the block.
            if !self.eat_op(Op::Semicolon) {
                self.skip_newlines();
            }
        }
        Ok(Rc::new(Block { stmts }))
    }

    fn parse_stmt(&mut self) -> Result<Stmt, Error> {
        // Statement keywords.
        match self.peek().clone() {
            Tok::Kw(Keyword::If) => return self.parse_if(),
            Tok::Kw(Keyword::While) => return self.parse_while(),
            Tok::Kw(Keyword::Do) => return self.parse_do(),
            Tok::Kw(Keyword::For) => return self.parse_for(),
            Tok::Kw(Keyword::Break) => {
                self.bump();
                return Ok(Stmt::Break);
            }
            Tok::Kw(Keyword::Continue) => {
                self.bump();
                return Ok(Stmt::Continue);
            }
            Tok::Kw(Keyword::Next) => {
                self.bump();
                return Ok(Stmt::Next);
            }
            Tok::Kw(Keyword::Nextfile) => {
                self.bump();
                // `nextfile` behaves as `next` for the single-input case.
                return Ok(Stmt::Next);
            }
            Tok::Kw(Keyword::Exit) => {
                self.bump();
                let e = self.parse_optional_expr()?;
                return Ok(Stmt::Exit(e));
            }
            Tok::Kw(Keyword::Return) => {
                self.bump();
                let e = self.parse_optional_expr()?;
                return Ok(Stmt::Return(e));
            }
            Tok::Kw(Keyword::Delete) => {
                self.bump();
                let array = self.expect_ident()?;
                let index = if self.eat_op(Op::LBracket) {
                    self.skip_newlines();
                    let e = self.expression()?;
                    self.skip_newlines();
                    self.expect_op(Op::RBracket)?;
                    Some(Rc::new(e))
                } else {
                    None
                };
                return Ok(Stmt::Delete { array, index });
            }
            // `print` and `printf` are ordinary identifiers until they appear
            // in statement position.
            Tok::Ident(name) if &*name == "print" => {
                self.bump();
                return self.parse_print();
            }
            Tok::Ident(name) if &*name == "printf" => {
                self.bump();
                return self.parse_printf();
            }
            Tok::Op(Op::LBrace) => {
                let b = self.parse_block()?;
                return Ok(Stmt::Block(b));
            }
            _ => {}
        }

        // Plain expression statement.
        let e = self.expression()?;
        Ok(Stmt::Expr(Rc::new(e)))
    }

    fn parse_optional_expr(&mut self) -> Result<Option<Rc<Expr>>, Error> {
        if matches!(self.peek(), Tok::Newline | Tok::Eof) || self.at_op(Op::Semicolon) || self.at_op(Op::RBrace)
        {
            return Ok(None);
        }
        Ok(Some(Rc::new(self.expression()?)))
    }

    fn parse_if(&mut self) -> Result<Stmt, Error> {
        self.expect_kw(Keyword::If)?;
        self.enter()?;
        let result = self.parse_if_inner();
        self.leave();
        result
    }

    fn parse_if_inner(&mut self) -> Result<Stmt, Error> {
        self.expect_op(Op::LParen)?;
        let cond = self.expression()?;
        self.expect_op(Op::RParen)?;
        let then = self.parse_braced_or_simple()?;
        // `if (a) b(); else c()` is legal awk, so the statement-terminating
        // semicolon may sit between the two branches.
        self.eat_op(Op::Semicolon);
        let otherwise = if self.eat_kw(Keyword::Else) { Some(self.parse_braced_or_simple()?) } else { None };
        Ok(Stmt::If { cond: Rc::new(cond), then, otherwise })
    }

    fn parse_braced_or_simple(&mut self) -> Result<Rc<Block>, Error> {
        self.skip_newlines();
        if self.at_op(Op::LBrace) {
            self.parse_block()
        } else {
            Ok(Rc::new(Block { stmts: vec![self.parse_stmt()?] }))
        }
    }

    fn parse_while(&mut self) -> Result<Stmt, Error> {
        self.expect_kw(Keyword::While)?;
        self.expect_op(Op::LParen)?;
        let cond = self.expression()?;
        self.expect_op(Op::RParen)?;
        let body = self.parse_braced_or_simple()?;
        Ok(Stmt::While { cond: Rc::new(cond), body })
    }

    fn parse_do(&mut self) -> Result<Stmt, Error> {
        self.expect_kw(Keyword::Do)?;
        let body = self.parse_braced_or_simple()?;
        self.skip_newlines();
        // `do b(); while (a)` — the statement terminator may precede `while`.
        self.eat_op(Op::Semicolon);
        self.skip_newlines();
        self.expect_kw(Keyword::While)?;
        self.expect_op(Op::LParen)?;
        let cond = self.expression()?;
        self.expect_op(Op::RParen)?;
        Ok(Stmt::DoWhile { body, cond: Rc::new(cond) })
    }

    fn parse_for(&mut self) -> Result<Stmt, Error> {
        self.expect_kw(Keyword::For)?;
        self.expect_op(Op::LParen)?;
        self.enter()?;
        let result = self.parse_for_inner();
        self.leave();
        result
    }

    fn parse_for_inner(&mut self) -> Result<Stmt, Error> {
        // `for (k in a)` — a single name, then `in`.
        if let Tok::Ident(name) = self.peek().clone() {
            if matches!(self.peek_at(1), Tok::Kw(Keyword::In)) {
                self.bump();
                self.bump();
                self.skip_newlines();
                let array = self.expect_ident()?;
                self.expect_op(Op::RParen)?;
                let body = self.parse_braced_or_simple()?;
                return Ok(Stmt::ForIn { var: Rc::new(Expr::Var(name)), array, body });
            }
        }

        let init = if self.at_op(Op::Semicolon) { None } else { Some(Box::new(self.parse_simple_stmt()?)) };
        self.expect_op(Op::Semicolon)?;
        let cond = if self.at_op(Op::Semicolon) { None } else { Some(Rc::new(self.expression()?)) };
        self.expect_op(Op::Semicolon)?;
        let step = if self.at_op(Op::RParen) { None } else { Some(Box::new(self.parse_simple_stmt()?)) };
        self.expect_op(Op::RParen)?;
        let body = self.parse_braced_or_simple()?;
        Ok(Stmt::For { init, cond, step, body })
    }

    /// The init and step of a `for` loop: an expression or a bare assignment.
    fn parse_simple_stmt(&mut self) -> Result<Stmt, Error> {
        let e = self.expression()?;
        Ok(Stmt::Expr(Rc::new(e)))
    }

    fn parse_print(&mut self) -> Result<Stmt, Error> {
        let mut args = Vec::new();
        // `print > "file"` has no arguments and prints `$0`.
        if !(self.at_op(Op::Gt) || self.at_op(Op::Comma) || self.at_op(Op::Semicolon)
            || self.at_op(Op::RBrace) || matches!(self.peek(), Tok::Newline | Tok::Eof))
        {
            self.no_gt += 1;
            loop {
                args.push(Rc::new(self.expression()?));
                if !self.eat_op(Op::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.no_gt -= 1;
        }
        let dest = self.parse_print_dest()?;
        Ok(Stmt::Print { args, dest })
    }

    /// `printf fmt, args` — the parentheses are optional in awk.
    fn parse_printf(&mut self) -> Result<Stmt, Error> {
        let parenthesised = self.eat_op(Op::LParen);
        self.no_gt += 1;
        let fmt = self.expression()?;
        let mut args = Vec::new();
        while self.eat_op(Op::Comma) {
            self.skip_newlines();
            args.push(Rc::new(self.expression()?));
        }
        self.no_gt -= 1;
        if parenthesised {
            self.skip_newlines();
            self.expect_op(Op::RParen)?;
        }
        let dest = self.parse_print_dest()?;
        Ok(Stmt::Printf { fmt: Rc::new(fmt), args, dest })
    }

    /// `> "file"` or `>> "file"` after a print statement. Bare `>` after
    /// `print` is redirection, not a comparison — matching awk.
    fn parse_print_dest(&mut self) -> Result<PrintDest, Error> {
        if self.eat_op(Op::Gt) {
            let append = self.eat_op(Op::Gt);
            let path = self.expression()?;
            Ok(PrintDest::File { path: Rc::new(path), append })
        } else {
            Ok(PrintDest::Stdout)
        }
    }

    // ---- expressions ---------------------------------------------------

    /// Assignment is right-associative and lowest precedence.
    fn expression(&mut self) -> Result<Expr, Error> {
        self.enter()?;
        let result = self.assignment();
        self.leave();
        result
    }

    fn assignment(&mut self) -> Result<Expr, Error> {
        let lhs = self.ternary()?;
        let op = match self.peek() {
            Tok::Op(Op::Assign) => Some(AssignOp::Assign),
            Tok::Op(Op::AddAssign) => Some(AssignOp::Add),
            Tok::Op(Op::SubAssign) => Some(AssignOp::Sub),
            Tok::Op(Op::MulAssign) => Some(AssignOp::Mul),
            Tok::Op(Op::DivAssign) => Some(AssignOp::Div),
            Tok::Op(Op::ModAssign) => Some(AssignOp::Mod),
            Tok::Op(Op::PowAssign) => Some(AssignOp::Pow),
            _ => None,
        };
        let Some(op) = op else { return Ok(lhs) };
        self.bump();
        if !lhs.is_lvalue() {
            return Err(self.error("left side of assignment is not assignable"));
        }
        let rhs = self.assignment()?;
        Ok(Expr::Assign { target: Box::new(lhs), op, value: Box::new(rhs) })
    }

    fn ternary(&mut self) -> Result<Expr, Error> {
        let cond = self.logical_or()?;
        if self.at_op(Op::Question) {
            self.bump();
            self.skip_newlines();
            let then = self.expression()?;
            self.skip_newlines();
            self.expect_op(Op::Colon)?;
            self.skip_newlines();
            let otherwise = self.ternary()?;
            return Ok(Expr::Ternary {
                cond: Box::new(cond),
                then: Box::new(then),
                otherwise: Box::new(otherwise),
            });
        }
        Ok(cond)
    }

    fn logical_or(&mut self) -> Result<Expr, Error> {
        let mut left = self.logical_and()?;
        while self.at_op(Op::OrOr) {
            self.bump();
            self.skip_newlines();
            let right = self.logical_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn logical_and(&mut self) -> Result<Expr, Error> {
        let mut left = self.in_expr()?;
        while self.at_op(Op::AndAnd) {
            self.bump();
            self.skip_newlines();
            let right = self.in_expr()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn in_expr(&mut self) -> Result<Expr, Error> {
        let left = self.match_expr()?;
        if self.at_kw(Keyword::In) {
            self.bump();
            let array = self.expect_ident()?;
            return Ok(Expr::In { key: Box::new(left), array });
        }
        Ok(left)
    }

    fn match_expr(&mut self) -> Result<Expr, Error> {
        let left = self.compare_lt()?;
        if self.at_op(Op::Tilde) {
            self.bump();
            let right = self.compare_lt()?;
            return Ok(Expr::Match { negated: false, left: Box::new(left), right: Box::new(right) });
        }
        if self.at_op(Op::Bang) && matches!(self.peek_at(1), Tok::Op(Op::Tilde)) {
            self.bump();
            self.bump();
            let right = self.compare_lt()?;
            return Ok(Expr::Match { negated: true, left: Box::new(left), right: Box::new(right) });
        }
        Ok(left)
    }

    fn compare_lt(&mut self) -> Result<Expr, Error> {
        let left = self.compare_eq()?;
        let op = match self.peek() {
            Tok::Op(Op::Lt) => Some(CmpOp::Lt),
            Tok::Op(Op::LtEq) => Some(CmpOp::Le),
            // A bare `>` ends a print argument list, so it is only a comparison
            // where redirection is not possible.
            Tok::Op(Op::Gt) if self.no_gt == 0 => Some(CmpOp::Gt),
            Tok::Op(Op::GtEq) => Some(CmpOp::Ge),
            _ => None,
        };
        let Some(op) = op else { return Ok(left) };
        self.bump();
        let right = self.compare_eq()?;
        Ok(Expr::Compare { op, left: Box::new(left), right: Box::new(right) })
    }

    fn compare_eq(&mut self) -> Result<Expr, Error> {
        let left = self.concat()?;
        let op = match self.peek() {
            Tok::Op(Op::EqEq) => Some(CmpOp::Eq),
            Tok::Op(Op::NotEq) => Some(CmpOp::Ne),
            _ => None,
        };
        let Some(op) = op else { return Ok(left) };
        self.bump();
        let right = self.concat()?;
        Ok(Expr::Compare { op, left: Box::new(left), right: Box::new(right) })
    }

    /// Adjacent expressions with no operator between them concatenate.
    fn concat(&mut self) -> Result<Expr, Error> {
        let mut left = self.additive()?;
        while self.starts_expr() {
            let right = self.additive()?;
            left = Expr::Concat(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, Error> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Op(Op::Plus) => ArithOp::Add,
                Tok::Op(Op::Minus) => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.bump();
            self.skip_newlines();
            let right = self.multiplicative()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, Error> {
        let mut left = self.power()?;
        loop {
            let op = match self.peek() {
                Tok::Op(Op::Star) => ArithOp::Mul,
                Tok::Op(Op::Slash) => ArithOp::Div,
                Tok::Op(Op::Percent) => ArithOp::Mod,
                _ => return Ok(left),
            };
            self.bump();
            self.skip_newlines();
            let right = self.power()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
    }

    /// `^` is right-associative and binds tighter than unary minus.
    fn power(&mut self) -> Result<Expr, Error> {
        let base = self.unary()?;
        if self.at_op(Op::Caret) {
            self.bump();
            self.skip_newlines();
            let exp = self.power()?;
            return Ok(Expr::Binary {
                op: ArithOp::Pow,
                left: Box::new(base),
                right: Box::new(exp),
            });
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<Expr, Error> {
        self.enter()?;
        let result = self.unary_inner();
        self.leave();
        result
    }

    fn unary_inner(&mut self) -> Result<Expr, Error> {
        match self.peek() {
            Tok::Op(Op::Bang) => {
                self.bump();
                // The operand is parsed at `power` level, not at `unary` level.
                // That is what makes `-2^2` mean `-(2^2)` = -4 rather than
                // `(-2)^2` = 4, while still allowing `2^-1`.
                let operand = self.power()?;
                Ok(Expr::Unary { op: UnOp::Not, operand: Box::new(operand) })
            }
            Tok::Op(Op::Minus) => {
                self.bump();
                let operand = self.power()?;
                Ok(Expr::Unary { op: UnOp::Neg, operand: Box::new(operand) })
            }
            Tok::Op(Op::Plus) => {
                self.bump();
                let operand = self.power()?;
                Ok(Expr::Unary { op: UnOp::Pos, operand: Box::new(operand) })
            }
            _ => self.postfix(),
        }
    }

    fn postfix(&mut self) -> Result<Expr, Error> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Tok::Op(Op::Incr) => {
                    self.bump();
                    if !e.is_lvalue() {
                        return Err(self.error("`++` needs an assignable operand"));
                    }
                    e = Expr::PostIncr { delta: 1.0, operand: Box::new(e) };
                }
                Tok::Op(Op::Decr) => {
                    self.bump();
                    if !e.is_lvalue() {
                        return Err(self.error("`--` needs an assignable operand"));
                    }
                    e = Expr::PostIncr { delta: -1.0, operand: Box::new(e) };
                }
                Tok::Op(Op::LBracket) => {
                    self.bump();
                    self.skip_newlines();
                    let saved = self.no_gt;
                    self.no_gt = 0;
                    // `a[i,j]` is `a[i SUBSEP j]` — the dimensions are joined
                    // with SUBSEP, exactly as `split(s, arr, SUBSEP)` undoes.
                    let mut idx = self.expression();
                    while self.at_op(Op::Comma) {
                        self.bump();
                        self.skip_newlines();
                        let next = self.expression();
                        idx = match (idx, next) {
                            (Ok(a), Ok(b)) => Ok(Expr::Concat(
                                Box::new(Expr::Concat(
                                    Box::new(a),
                                    Box::new(Expr::Var(Rc::from("SUBSEP"))),
                                )),
                                Box::new(b),
                            )),
                            (Err(e), _) | (_, Err(e)) => Err(e),
                        };
                    }
                    self.no_gt = saved;
                    let idx = idx?;
                    self.skip_newlines();
                    self.expect_op(Op::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                // `(` after a name is a call and is consumed in `primary`. Anywhere
                // else it means concatenation, so `1 (2)` is `1 " " "2"`.
                Tok::Op(Op::LParen) if !matches!(e, Expr::Var(_)) => {
                    self.bump();
                    self.skip_newlines();
                    let inner = self.expression()?;
                    self.skip_newlines();
                    self.expect_op(Op::RParen)?;
                    e = Expr::Concat(Box::new(e), Box::new(Expr::Grouping(Box::new(inner))));
                }
                _ => return Ok(e),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, Error> {
        match self.peek().clone() {
            Tok::Num(n) => {
                self.bump();
                Ok(Expr::Num(n))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Str(s))
            }
            Tok::Regex(r) => {
                self.bump();
                Ok(Expr::Regex(r))
            }
            Tok::Op(Op::Dollar) => {
                self.bump();
                let operand = self.primary()?;
                Ok(Expr::Field(Box::new(operand)))
            }
            Tok::Op(Op::LParen) => {
                self.bump();
                self.skip_newlines();
                // Parentheses restore `>` to its usual meaning, so that
                // `print (1 > 0)` compares rather than redirecting.
                let saved = self.no_gt;
                self.no_gt = 0;
                let e = self.expression();
                self.no_gt = saved;
                let e = e?;
                self.skip_newlines();
                self.expect_op(Op::RParen)?;
                Ok(Expr::Grouping(Box::new(e)))
            }
            Tok::Op(Op::Incr) => {
                self.bump();
                let operand = self.unary()?;
                if !operand.is_lvalue() {
                    return Err(self.error("`++` needs an assignable operand"));
                }
                Ok(Expr::PreIncr { delta: 1.0, operand: Box::new(operand) })
            }
            Tok::Op(Op::Decr) => {
                self.bump();
                let operand = self.unary()?;
                if !operand.is_lvalue() {
                    return Err(self.error("`--` needs an assignable operand"));
                }
                Ok(Expr::PreIncr { delta: -1.0, operand: Box::new(operand) })
            }
            Tok::Ident(name) => match &*name {
                "getline" => {
                    self.bump();
                    self.parse_getline_tail()
                }
                _ => {
                    self.bump();
                    // A name directly followed by `(` is a call.
                    if self.at_op(Op::LParen) {
                        self.bump();
                        self.skip_newlines();
                        let mut args = Vec::new();
                        if !self.at_op(Op::RParen) {
                            loop {
                                args.push(Rc::new(self.expression()?));
                                self.skip_newlines();
                                if !self.eat_op(Op::Comma) {
                                    break;
                                }
                                self.skip_newlines();
                            }
                        }
                        self.skip_newlines();
                        self.expect_op(Op::RParen)?;
                        Ok(Expr::Call { name, args })
                    } else {
                        Ok(Expr::Var(name))
                    }
                }
            },
            other => Err(self.error(format!("unexpected {}", other.describe()))),
        }
    }

    /// Everything after the `getline` keyword.
    fn parse_getline_tail(&mut self) -> Result<Expr, Error> {
        self.skip_newlines();
        // `getline < file`
        if self.at_op(Op::Lt) {
            self.bump();
            self.skip_newlines();
            let path = self.expression()?;
            return Ok(Expr::Getline(Getline::FromFile(Rc::new(path))));
        }
        // `getline var`
        if let Tok::Ident(name) = self.peek().clone() {
            self.bump();
            if self.at_op(Op::Lt) {
                self.bump();
                self.skip_newlines();
                let path = self.expression()?;
                return Ok(Expr::Getline(Getline::FromFileIntoVar(
                    Rc::new(Expr::Var(name)),
                    Rc::new(path),
                )));
            }
            return Ok(Expr::Getline(Getline::IntoVar(Rc::new(Expr::Var(name)))));
        }
        Ok(Expr::Getline(Getline::FromRecord))
    }

    /// Whether the current token can begin an operand — used to find implicit
    /// string concatenation.
    ///
    /// `-` and `!` are deliberately excluded: `x -1` is a subtraction, not a
    /// concatenation with negative one, and `a && !b` is handled by the
    /// operator levels above.
    fn starts_expr(&self) -> bool {
        matches!(
            self.peek(),
            Tok::Num(_) | Tok::Str(_) | Tok::Ident(_) | Tok::Regex(_) | Tok::Op(Op::Dollar) | Tok::Op(Op::LParen)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(src: &str) -> Expr {
        parse_expression(src).unwrap_or_else(|e| panic!("{src:?}: {e}"))
    }

    fn parse(src: &str) -> Program {
        parse_program(src).unwrap_or_else(|e| panic!("{src:?}: {e}"))
    }

    /// Structural equality helper, since `Rc<Expr>` needs a manual compare.
    fn same(a: &Expr, b: &Expr) -> bool {
        format!("{a:?}") == format!("{b:?}")
    }

    fn boxed(e: Expr) -> Box<Expr> {
        Box::new(e)
    }

    #[test]
    fn unary_minus_binds_looser_than_power() {
        // -2^2 is -(2^2) = -4, verified against all three oracles.
        let e = expr("-2^2");
        assert!(matches!(&e, Expr::Unary { op: UnOp::Neg, .. }));
        match &e {
            Expr::Unary { operand, .. } => {
                assert!(matches!(&**operand, Expr::Binary { op: ArithOp::Pow, .. }));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn power_is_right_associative() {
        // 2^3^2 is 2^(3^2) = 512.
        let e = expr("2^3^2");
        match &e {
            Expr::Binary { op: ArithOp::Pow, left, right } => {
                assert!(matches!(&**left, Expr::Num(2.0)));
                assert!(matches!(&**right, Expr::Binary { op: ArithOp::Pow, .. }));
            }
            _ => panic!("expected Pow, got {e:?}"),
        }
    }

    #[test]
    fn arithmetic_precedence() {
        assert!(same(&expr("1+2*3"), &Expr::Binary {
            op: ArithOp::Add,
            left: boxed(Expr::Num(1.0)),
            right: boxed(Expr::Binary {
                op: ArithOp::Mul,
                left: boxed(Expr::Num(2.0)),
                right: boxed(Expr::Num(3.0))
            })
        }));
        // 2^2*3 is (2^2)*3 = 12.
        assert!(matches!(&expr("2^2*3"), Expr::Binary { op: ArithOp::Mul, left, .. }
            if matches!(&**left, Expr::Binary { op: ArithOp::Pow, .. })));
    }

    #[test]
    fn concat_binds_tighter_than_comparison() {
        assert!(same(&expr("1 \" \" 2 == \"1 2\""), &Expr::Compare {
            op: CmpOp::Eq,
            left: boxed(Expr::Concat(
                boxed(Expr::Concat(boxed(Expr::Num(1.0)), boxed(Expr::Str(" ".into())))),
                boxed(Expr::Num(2.0))
            )),
            right: boxed(Expr::Str("1 2".into()))
        }));
    }

    #[test]
    fn minus_is_not_a_concatenation_starter() {
        // `x -1` is a subtraction, not `x` concatenated with `-1`.
        assert!(matches!(&expr("x -1"), Expr::Binary { op: ArithOp::Sub, .. }));
    }

    #[test]
    fn field_and_subscript_chains() {
        assert!(same(&expr("$1"), &Expr::Field(boxed(Expr::Num(1.0)))));
        assert!(same(&expr("a[i]"), &Expr::Index(boxed(Expr::Var("a".into())), boxed(Expr::Var("i".into())))));
        assert!(same(&expr("a[1][2]"), &Expr::Index(
            boxed(Expr::Index(boxed(Expr::Var("a".into())), boxed(Expr::Num(1.0)))),
            boxed(Expr::Num(2.0))
        )));
    }

    #[test]
    fn assignment_is_right_associative() {
        assert!(same(&expr("a=b=c"), &Expr::Assign {
            target: boxed(Expr::Var("a".into())),
            op: AssignOp::Assign,
            value: boxed(Expr::Assign {
                target: boxed(Expr::Var("b".into())),
                op: AssignOp::Assign,
                value: boxed(Expr::Var("c".into()))
            })
        }));
    }

    #[test]
    fn assignment_to_a_non_lvalue_is_rejected() {
        assert!(parse_expression("1 = 2").is_err());
        assert!(parse_expression("\"a\" += 1").is_err());
    }

    #[test]
    fn rules_functions_and_blocks() {
        let p = parse("BEGIN { x = 1 } /re/ { print } END { y = 2 } function f(a, b) { return a }");
        assert!(matches!(p.items[0], Item::Begin(_)));
        assert!(matches!(p.items[1], Item::Rule { pattern: Pattern::Expr(_), action: Some(_) }));
        assert!(matches!(p.items[2], Item::End(_)));
        assert!(matches!(p.items[3], Item::Func(_)));
        assert!(p.funcs.contains_key("f"));
        assert_eq!(p.funcs["f"].params.len(), 2);
    }

    #[test]
    fn a_rule_with_no_action_prints_the_record() {
        let p = parse("/re/");
        match &p.items[0] {
            Item::Rule { action, .. } => assert!(action.is_none()),
            _ => panic!(),
        }
    }

    #[test]
    fn control_flow_statements() {
        parse("BEGIN { if (a) b(); else c() }");
        parse("BEGIN { if (a) { b() } else { c() } }");
        parse("BEGIN { while (a) b() }");
        parse("BEGIN { do b(); while (a) }");
        parse("BEGIN { for (i = 0; i < 3; i++) b() }");
        parse("BEGIN { for (k in a) b() }");
        parse("BEGIN { while (1) { break; continue } }");
    }

    #[test]
    fn print_redirection_is_not_a_comparison() {
        let p = parse("BEGIN { print > \"f\" }");
        match &p.items[0] {
            Item::Begin(b) => match &b.stmts[0] {
                Stmt::Print { args, dest } => {
                    assert!(args.is_empty());
                    assert!(matches!(dest, PrintDest::File { append: false, .. }));
                }
                other => panic!("{other:?}"),
            },
            _ => panic!(),
        }
        // With an argument, the bare `>` still means redirection.
        let p = parse("BEGIN { print 1 > \"f\" }");
        match &p.items[0] {
            Item::Begin(b) => match &b.stmts[0] {
                Stmt::Print { args, dest } => {
                    assert_eq!(args.len(), 1);
                    assert!(matches!(dest, PrintDest::File { .. }));
                }
                other => panic!("{other:?}"),
            },
            _ => panic!(),
        }
        // Parenthesised, `>` is a comparison.
        let p = parse("BEGIN { print (1 > 0) }");
        match &p.items[0] {
            Item::Begin(b) => match &b.stmts[0] {
                Stmt::Print { args, dest } => {
                    assert!(matches!(dest, PrintDest::Stdout));
                    assert!(matches!(&*args[0], Expr::Grouping(g) if matches!(&**g, Expr::Compare { .. })));
                }
                other => panic!("{other:?}"),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn getline_forms() {
        assert!(matches!(&expr("getline"), Expr::Getline(Getline::FromRecord)));
        assert!(matches!(&expr("getline v"), Expr::Getline(Getline::IntoVar(_))));
        assert!(matches!(&expr("getline < f"), Expr::Getline(Getline::FromFile(_))));
        assert!(matches!(&expr("getline v < f"), Expr::Getline(Getline::FromFileIntoVar(_, _))));
    }

    #[test]
    fn range_patterns() {
        let p = parse("/a/,/b/ { print }");
        match &p.items[0] {
            Item::Rule { pattern: Pattern::Range(_, _), .. } => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn in_operator() {
        assert!(matches!(&expr("k in a"), Expr::In { .. }));
    }

    #[test]
    fn deep_nesting_is_rejected_rather_than_overflowing() {
        let src = format!("{}{}", "(".repeat(5000), "1");
        // A stack overflow cannot be caught by Result, so this must be a clean
        // parse error.
        assert!(parse_expression(&src).is_err());
    }

    #[test]
    fn syntax_errors_report_a_position() {
        let e = parse_program("BEGIN { print 1 } }").unwrap_err();
        assert_eq!(e.line, Some(1));
    }
}