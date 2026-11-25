//! The eawk abstract syntax tree.
//!
//! A parsed program is immutable and cheap to share, which is what lets
//! [`crate::Interp`] be compiled once and instantiated many times. All mutable
//! state — variables, fields, open files — lives in the interpreter instance,
//! never in here.

use std::collections::HashMap;
use std::rc::Rc;

/// A whole parsed awk program.
#[derive(Debug, Default)]
pub struct Program {
    /// `BEGIN`/`END`/pattern-action rules and function definitions, in source
    /// order.
    pub items: Vec<Item>,
    /// Function definitions by name, for the call path.
    pub funcs: HashMap<Rc<str>, Rc<FuncDef>>,
}

/// One top-level item.
#[derive(Debug)]
pub enum Item {
    /// `BEGIN { ... }`
    Begin(Rc<Block>),
    /// An `END { ... }` rule.
    End(Rc<Block>),
    /// A pattern-action rule. A `None` action means "print `$0`".
    Rule { pattern: Pattern, action: Option<Rc<Block>> },
    /// A `function f(a, b) { ... }` definition.
    Func(Rc<FuncDef>),
}

/// The left-hand side of a rule.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// Bare `{ ... }` or a bare expression: always runs.
    Always,
    /// An expression that selects records.
    Expr(Rc<Expr>),
    /// `pat1, pat2` — run from the record matching `pat1` through the record
    /// matching `pat2`, inclusive.
    Range(Rc<Expr>, Rc<Expr>),
}

/// A user-defined function.
#[derive(Debug, Clone)]
pub struct FuncDef {
    /// Function name.
    pub name: Rc<str>,
    /// Declared parameters. Parameters past the supplied arguments act as
    /// locals, per the usual awk convention.
    pub params: Vec<Rc<str>>,
    /// Function body.
    pub body: Rc<Block>,
}

/// A brace-delimited sequence of statements.
#[derive(Debug)]
pub struct Block {
    /// The statements, in order.
    pub stmts: Vec<Stmt>,
}

impl Block {
    /// An empty block.
    pub fn empty() -> Rc<Block> {
        Rc::new(Block { stmts: Vec::new() })
    }
}

/// A statement.
#[derive(Debug)]
pub enum Stmt {
    /// An expression evaluated for its value.
    Expr(Rc<Expr>),
    /// `print` with an optional redirection target.
    Print { args: Vec<Rc<Expr>>, dest: PrintDest },
    /// `printf` with an optional redirection target.
    Printf { fmt: Rc<Expr>, args: Vec<Rc<Expr>>, dest: PrintDest },
    /// `if (c) a else b`
    If { cond: Rc<Expr>, then: Rc<Block>, otherwise: Option<Rc<Block>> },
    /// `while (c) body`
    While { cond: Rc<Expr>, body: Rc<Block> },
    /// `do body while (c)`
    DoWhile { body: Rc<Block>, cond: Rc<Expr> },
    /// `for (init; cond; step) body`
    For { init: Option<Box<Stmt>>, cond: Option<Rc<Expr>>, step: Option<Box<Stmt>>, body: Rc<Block> },
    /// `for (k in arr) body`
    ForIn { var: Rc<Expr>, array: Rc<str>, body: Rc<Block> },
    /// A nested `{ ... }`.
    Block(Rc<Block>),
    /// `next`
    Next,
    /// `exit` or `exit expr`
    Exit(Option<Rc<Expr>>),
    /// `return` or `return expr`
    Return(Option<Rc<Expr>>),
    /// `break`
    Break,
    /// `continue`
    Continue,
    /// `delete arr[i]` or `delete arr`
    Delete { array: Rc<str>, index: Option<Rc<Expr>> },
}

/// Where a `print` or `printf` sends its output.
#[derive(Debug)]
pub enum PrintDest {
    /// Standard output.
    Stdout,
    /// `> expr` or `>> expr`.
    File { path: Rc<Expr>, append: bool },
}

/// Binary arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `^`
    Pow,
}

/// Binary comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `==`
    Eq,
    /// `!=`
    Ne,
}

/// An assignment operator. `Assign` is plain `=`; the rest are compound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`
    Assign,
    /// `+=`
    Add,
    /// `-=`
    Sub,
    /// `*=`
    Mul,
    /// `/=`
    Div,
    /// `%=`
    Mod,
    /// `^=`
    Pow,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// `!`
    Not,
    /// `-`
    Neg,
    /// `+`
    Pos,
}

/// An expression.
#[derive(Debug)]
pub enum Expr {
    /// A numeric literal.
    Num(f64),
    /// A string literal.
    Str(Rc<str>),
    /// A `/.../` regular expression literal.
    Regex(Rc<str>),
    /// A variable reference. For an array this is only valid inside a subscript.
    Var(Rc<str>),
    /// `$expr`
    Field(Box<Expr>),
    /// `base[subscript]`
    Index(Box<Expr>, Box<Expr>),
    /// An lvalue assignment; `op` is `None` for plain `=`.
    Assign { target: Box<Expr>, op: AssignOp, value: Box<Expr> },
    /// Arithmetic.
    Binary { op: ArithOp, left: Box<Expr>, right: Box<Expr> },
    /// Comparison. Whether this is numeric or string is decided at run time by
    /// the types of the operands — see [`crate::value::compare`].
    Compare { op: CmpOp, left: Box<Expr>, right: Box<Expr> },
    /// `left ~ right` or `left !~ right`
    Match { negated: bool, left: Box<Expr>, right: Box<Expr> },
    /// String concatenation.
    Concat(Box<Expr>, Box<Expr>),
    /// `left && right`, short-circuiting.
    And(Box<Expr>, Box<Expr>),
    /// `left || right`, short-circuiting.
    Or(Box<Expr>, Box<Expr>),
    /// Unary.
    Unary { op: UnOp, operand: Box<Expr> },
    /// `x++` / `x--`
    PostIncr { delta: f64, operand: Box<Expr> },
    /// `++x` / `--x`
    PreIncr { delta: f64, operand: Box<Expr> },
    /// `cond ? a : b`
    Ternary { cond: Box<Expr>, then: Box<Expr>, otherwise: Box<Expr> },
    /// `key in array`
    In { key: Box<Expr>, array: Rc<str> },
    /// A function call, either user-defined or builtin.
    Call { name: Rc<str>, args: Vec<Rc<Expr>> },
    /// A parenthesised expression, kept so that precedence survives parsing.
    Grouping(Box<Expr>),
    /// `getline`, its variants, and the lvalue it assigns to.
    Getline(Getline),
}

/// The several shapes of `getline`.
#[derive(Debug)]
pub enum Getline {
    /// `getline` — into `$0`.
    FromRecord,
    /// `getline var` — from the main input, into a variable.
    IntoVar(Rc<Expr>),
    /// `getline < file`
    FromFile(Rc<Expr>),
    /// `getline var < file`
    FromFileIntoVar(Rc<Expr>, Rc<Expr>),
    /// `cmd | getline` and `cmd | getline var`
    PipeGetline { cmd: Rc<Expr>, var: Option<Rc<Expr>> },
}

impl Expr {
    /// True if this expression can be assigned to.
    pub fn is_lvalue(&self) -> bool {
        match self {
            Expr::Var(_) | Expr::Field(_) | Expr::Index(_, _) => true,
            Expr::Grouping(e) => e.is_lvalue(),
            _ => false,
        }
    }
}