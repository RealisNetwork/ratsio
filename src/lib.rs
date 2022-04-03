#[macro_use]
extern crate nom;
#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate derive_builder;

pub mod protocol {
    include!(concat!(env!("OUT_DIR"), "/pb.rs"));
}

pub mod error;
pub mod nats_client;
pub mod net;
pub mod nuid;
pub mod ops;
pub mod parser;
pub mod stan_client;

pub use error::RatsioError;
pub use nats_client::{NatsClient, NatsClientOptions, NatsMessage, NatsSid};
pub use stan_client::{StanClient, StanMessage, StanOptions, StanSid, StartPosition};
