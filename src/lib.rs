#![allow(clippy::enum_variant_names)]
#![allow(clippy::large_enum_variant)]

pub mod chain;
mod channel_acceptor;
pub mod config;
mod events;
mod fees;
mod keys;
pub mod logger;
pub mod models;
pub mod node;
pub mod onchain;
mod server;

pub mod lnrpc {
    tonic::include_proto!("lnrpc");
}

pub mod invoicesrpc {
    tonic::include_proto!("invoicesrpc");
}

pub mod lndkrpc {
    tonic::include_proto!("lndkrpc");
}

pub mod routerrpc {
    tonic::include_proto!("routerrpc");
}

pub mod signrpc {
    tonic::include_proto!("signrpc");
}

pub mod walletrpc {
    tonic::include_proto!("walletrpc");
}
