use bitcoincore_rpc::bitcoincore_rpc_json::EstimateMode;
use bitcoincore_rpc::RpcApi;
use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// The minimum feerate we are allowed to send, as specify by LDK.
const MIN_FEERATE: u32 = 253;

/// The RLD fee estimator
#[derive(Debug, Clone)]
pub struct RldFeeEstimator {
    /// The RPC client to the Bitcoin Core node
    pub rpc: Arc<bitcoincore_rpc::Client>,
    cache: Arc<RwLock<HashMap<ConfirmationTarget, u32>>>,
}

impl RldFeeEstimator {
    /// Create a new RLD fee estimator
    pub fn new(rpc: Arc<bitcoincore_rpc::Client>) -> anyhow::Result<Self> {
        let cache = Self::populate_cache(&rpc)?;
        Ok(Self {
            rpc,
            cache: Arc::new(RwLock::new(cache)),
        })
    }

    fn populate_cache(
        rpc: &bitcoincore_rpc::Client,
    ) -> anyhow::Result<HashMap<ConfirmationTarget, u32>> {
        let mut hash_map = HashMap::with_capacity(6);

        let mempoolmin_estimate = {
            let btc_per_kvbyte = rpc.get_mempool_info()?.mempool_min_fee.to_btc();
            convert_fee_rate(btc_per_kvbyte)
        };

        let background_estimate = {
            let resp = rpc.estimate_smart_fee(144, Some(EstimateMode::Economical))?;
            match resp.fee_rate {
                Some(fee) => convert_fee_rate(fee.to_btc()),
                None => MIN_FEERATE,
            }
        };

        let normal_estimate = {
            let resp = rpc.estimate_smart_fee(18, Some(EstimateMode::Economical))?;
            match resp.fee_rate {
                Some(fee) => convert_fee_rate(fee.to_btc()),
                None => 2_000,
            }
        };

        let high_prio_estimate = {
            let resp = rpc.estimate_smart_fee(6, Some(EstimateMode::Economical))?;
            match resp.fee_rate {
                Some(fee) => convert_fee_rate(fee.to_btc()),
                None => 5_000,
            }
        };

        let very_high_prio_estimate = {
            let resp = rpc.estimate_smart_fee(2, Some(EstimateMode::Conservative))?;
            match resp.fee_rate {
                Some(fee) => convert_fee_rate(fee.to_btc()),
                None => 50_000,
            }
        };

        hash_map.insert(
            ConfirmationTarget::MaximumFeeEstimate,
            very_high_prio_estimate,
        );
        hash_map.insert(ConfirmationTarget::UrgentOnChainSweep, high_prio_estimate);
        hash_map.insert(
            ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
            mempoolmin_estimate,
        );
        hash_map.insert(
            ConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
            background_estimate - 250,
        );
        hash_map.insert(ConfirmationTarget::AnchorChannelFee, background_estimate);
        hash_map.insert(ConfirmationTarget::NonAnchorChannelFee, normal_estimate);
        hash_map.insert(ConfirmationTarget::ChannelCloseMinimum, normal_estimate);
        hash_map.insert(ConfirmationTarget::OutputSpendingFee, normal_estimate);

        Ok(hash_map)
    }

    pub fn get_low_fee_rate(&self) -> u32 {
        // MinAllowedNonAnchorChannelRemoteFee is a fee rate we expect to get slowly
        self.get_est_sat_per_1000_weight(ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee)
    }

    pub fn get_normal_fee_rate(&self) -> u32 {
        // NonAnchorChannelFee is a fee rate we expect to be confirmed in 6 blocks
        self.get_est_sat_per_1000_weight(ConfirmationTarget::NonAnchorChannelFee)
    }

    pub fn get_high_fee_rate(&self) -> u32 {
        // UrgentOnChainSweep is the highest fee rate we have, so use that
        self.get_est_sat_per_1000_weight(ConfirmationTarget::UrgentOnChainSweep)
    }
}

impl FeeEstimator for RldFeeEstimator {
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        let cache = self.cache.read().expect("Could not read from cache");
        match cache.get(&confirmation_target).copied() {
            Some(fee) => fee,
            None => {
                drop(cache);
                let mut cache = self.cache.write().expect("Could not write to cache");
                let new_cache = Self::populate_cache(&self.rpc).expect("Could not populate cache");
                *cache = new_cache;
                cache[&confirmation_target]
            }
        }
    }
}

fn convert_fee_rate(fee_rate: f64) -> u32 {
    (fee_rate * 100_000_000.0 / 4.0).round() as u32
}
