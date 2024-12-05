//! src/routes/mod.rs
mod error_chain_fmt;
mod health_check;
mod home;
mod login;
mod newsletter;
mod subscriptions;
mod subscriptions_confirm;

pub use error_chain_fmt::*;
pub use health_check::*;
pub use home::*;
pub use login::*;
pub use newsletter::*;
pub use subscriptions::*;
pub use subscriptions_confirm::*;
