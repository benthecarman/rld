use clap::Parser;
use lightning::ln::msgs::SocketAddress;
use lightning::util::ser::Hostname;

/// Rust Lightning Daemon
#[derive(Parser, Debug, Clone)]
#[command(version, author, about)]
pub struct Config {
    /// Location of keys files and data
    #[clap(default_value = ".", long)]
    pub data_dir: String,
    /// Postgres connection string for payment information
    #[clap(long)]
    pub pg_url: String,
    /// Port to listen on for lightning connections
    #[clap(default_value_t = 9735, long)]
    pub port: u16,

    /// Bitcoin network to use
    #[clap(default_value = "signet", long)]
    pub network: String,

    /// Alias for the node
    #[clap(long)]
    pub alias: Option<String>,
    /// IP address to advertise to the network
    #[clap(long)]
    pub external_ip: Option<String>,

    /// Bind address for the RPC server
    #[clap(default_value = "127.0.0.1", long)]
    pub rpc_bind: String,
    /// Port for the RPC server
    #[clap(default_value_t = 9999, long)]
    pub rpc_port: u16,

    /// Host address of the bitcoind the RPC server
    #[clap(default_value = "127.0.0.1", long)]
    pub bitcoind_host: String,
    /// Port of the bitcoind RPC server
    #[clap(default_value_t = 38332, long)]
    pub bitcoind_port: u16,
    /// bitcoind the RPC server user
    #[clap(long)]
    pub bitcoind_rpc_user: String,
    /// bitcoind the RPC server password
    #[clap(long)]
    pub bitcoind_rpc_password: String,
}

impl Config {
    pub fn network(&self) -> bitcoin::Network {
        match self.network.as_str() {
            "mainnet" => bitcoin::Network::Bitcoin,
            "bitcoin" => bitcoin::Network::Bitcoin,
            "testnet" => bitcoin::Network::Testnet,
            "regtest" => bitcoin::Network::Regtest,
            "signet" => bitcoin::Network::Signet,
            _ => panic!("Invalid network"),
        }
    }

    pub fn list_address(&self) -> Option<SocketAddress> {
        self.external_ip
            .as_ref()
            .map(|hostname| SocketAddress::Hostname {
                hostname: Hostname::try_from(hostname.clone()).expect("invalid external ip"),
                port: self.port,
            })
    }

    pub fn alias(&self) -> &str {
        self.alias
            .as_deref()
            .unwrap_or("Rust Lightning Daemon")
    }
}
