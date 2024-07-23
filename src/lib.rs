pub mod chain;
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

pub mod signrpc {
	tonic::include_proto!("signrpc");
}

pub mod walletrpc {
	tonic::include_proto!("walletrpc");
}
