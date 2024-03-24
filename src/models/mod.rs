use crate::logger::RldLogger;
use crate::node::NetworkGraph;
use bitcoin::Network;
use diesel_migrations::{embed_migrations, EmbeddedMigrations};
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringDecayParameters};
use lightning::util::ser::ReadableArgs;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

pub mod invoice;
pub mod payment;
mod schema;

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

pub(crate) fn read_network_graph(
    path: &Path,
    network: Network,
    logger: Arc<RldLogger>,
) -> NetworkGraph {
    if let Ok(file) = File::open(path) {
        if let Ok(graph) = NetworkGraph::read(&mut BufReader::new(file), logger.clone()) {
            return graph;
        }
    }
    NetworkGraph::new(network, logger)
}

pub(crate) fn read_scorer(
    path: &Path,
    graph: Arc<NetworkGraph>,
    logger: Arc<RldLogger>,
) -> ProbabilisticScorer<Arc<NetworkGraph>, Arc<RldLogger>> {
    let params = ProbabilisticScoringDecayParameters::default();
    if let Ok(file) = File::open(path) {
        let args = (params, Arc::clone(&graph), Arc::clone(&logger));
        if let Ok(scorer) = ProbabilisticScorer::read(&mut BufReader::new(file), args) {
            return scorer;
        }
    }
    ProbabilisticScorer::new(params, graph, logger)
}
