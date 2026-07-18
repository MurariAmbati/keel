//! KEEL's SQL front end — lexer, AST, and a hand-written recursive-descent parser
//! over the freeze grammar (§6.1, D10). The binder, planner, and executor live in
//! the engine crate that mounts this on real storage.

pub mod ast;
pub mod gen;
pub mod lex;
pub mod parse;
pub mod refengine;

pub use ast::*;
pub use parse::{parse_expr, parse_statement, ParseError};
pub use refengine::{ExecError, MemDb, ResultSet, Row};
