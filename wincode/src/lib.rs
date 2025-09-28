#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
pub mod error;
pub use error::{Error, Result};
pub mod io;
pub mod len;
mod schema;
pub use schema::*;
mod serde;
pub use serde::*;

#[doc(hidden)]
pub mod __private {
    #[doc(hidden)]
    pub use paste::paste;
}
