pub mod action;
pub mod content;
pub mod entities;
pub mod frame;
pub mod ids;
pub(crate) mod macros;
pub mod message;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid {type_name}: {value:?}")]
pub struct ParseEnumError {
    pub type_name: &'static str,
    pub value: String,
}
