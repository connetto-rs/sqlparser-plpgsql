#![doc = include_str!("../README.md")]
#![no_std]
extern crate alloc;

// libtest and proptest need std, while the library itself never does.
#[cfg(test)]
extern crate std;

pub mod context;
pub mod error;
pub mod preprocessor;
pub mod scanner;

#[cfg(feature = "body-parse")]
pub mod body;
#[cfg(feature = "body-parse")]
pub mod embedded;
pub use context::{PlPgSqlContext, UuidFirstUse, VariableBinding, VariableDeclaration};
pub use error::Error;
pub use preprocessor::PlPgSqlPreprocessor;
pub use scanner::{Region, Scanner};

#[cfg(feature = "body-parse")]
pub use body::parse_body;
#[cfg(feature = "body-parse")]
pub use embedded::embedded_sql;
