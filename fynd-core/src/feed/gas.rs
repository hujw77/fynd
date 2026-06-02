use chrono::Utc;
use metrics::{counter, gauge};
use tokio::sync::mpsc;
use tracing::warn;
use tycho_simulation::{tycho_core::traits::FeePriceGetter, tycho_ethereum::gas::BlockGasPrice};

use crate::feed::{market_data::MarketData, DataFeedError};

// TODO: Refactor gas price fetching into a `DerivedComputation`.
pub(crate) struct GasPriceFetcher<C: FeePriceGetter<FeePrice = BlockGasPrice>> {
    client: C,
    signal_rx: mpsc::Receiver<()>,
    shared_market_data: MarketData,
}

impl<C: FeePriceGetter<FeePrice = BlockGasPrice>> GasPriceFetcher<C> {
    pub(crate) fn new(client: C, shared_market_data: MarketData) -> (Self, mpsc::Sender<()>) {
        let (signal_tx, signal_rx) = mpsc::channel(5);
        (Self { client, signal_rx, shared_market_data }, signal_tx)
    }

    pub(crate) async fn run(&mut self) -> Result<(), DataFeedError> {
        loop {
            self.signal_rx
                .recv()
                .await
                .ok_or(DataFeedError::GasPriceFetcherError("Trigger channel closed".to_string()))?;

            let fee_price = match self.client.get_latest_fee_price().await {
                Ok(price) => price,
                Err(e) => {
                    counter!("gas_price_fetch_failures_total").increment(1);
                    warn!(error = ?e, "Failed to fetch gas price, skipping update. Configure --gas-price-stale-threshold-secs to surface this in health checks");
                    continue;
                }
            };

            let mut lock = self.shared_market_data.write().await;
            let update_block_number = fee_price.block_number;
            lock.update_gas_price(fee_price);
            if let Some(last_block_info) = lock.last_updated() {
                let update_lag_ms =
                    Utc::now().timestamp_millis() - (last_block_info.timestamp() as i64 * 1000);
                gauge!("gas_price_update_lag_ms").set(update_lag_ms as f64);

                if last_block_info
                    .number()
                    .abs_diff(update_block_number) >
                    3
                {
                    warn!(
                        gas_price_block = update_block_number,
                        last_tycho_block = last_block_info.number(),
                        "gas price is more than 3 blocks ahead of the last Tycho update"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use async_trait::async_trait;
    use num_bigint::BigUint;
    use tycho_simulation::tycho_ethereum::gas::{BlockGasPrice, GasPrice};

    use super::*;

    /// Mock client that returns errors for the first `fail_count` calls,
    /// then succeeds with a fixed gas price.
    struct MockFeePriceGetter {
        call_count: AtomicUsize,
        fail_count: usize,
    }

    impl MockFeePriceGetter {
        fn new(fail_count: usize) -> Self {
            Self { call_count: AtomicUsize::new(0), fail_count }
        }
    }

    #[async_trait]
    impl FeePriceGetter for MockFeePriceGetter {
        type Error = String;
        type FeePrice = BlockGasPrice;

        async fn get_latest_fee_price(&self) -> Result<BlockGasPrice, String> {
            let call = self
                .call_count
                .fetch_add(1, Ordering::SeqCst);
            if call < self.fail_count {
                return Err(format!("RPC timeout (call {})", call));
            }
            Ok(BlockGasPrice {
                block_number: 100 + call as u64,
                block_hash: Default::default(),
                block_timestamp: 1_700_000_000,
                pricing: GasPrice::Legacy { gas_price: BigUint::from(30_000_000_000u64) },
            })
        }
    }

    /// Sends a signal and waits up to `timeout` for `predicate` to hold.
    async fn trigger_and_wait(
        signal_tx: &mpsc::Sender<()>,
        market_data: &MarketData,
        timeout: Duration,
        predicate: impl Fn(&crate::feed::market_data::MarketDataView<'_>) -> bool,
    ) {
        signal_tx
            .send(())
            .await
            .expect("signal send failed");

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if predicate(&market_data.read().await) {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for gas price update");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn fetch_error_does_not_crash() {
        let market_data = MarketData::new_shared();
        let (mut fetcher, signal_tx) =
            GasPriceFetcher::new(MockFeePriceGetter::new(1), market_data.clone());

        let handle = tokio::spawn(async move { fetcher.run().await });

        // First signal → mock errors → gas price stays None, fetcher keeps running.
        // We send and yield; no ack to wait for.
        signal_tx
            .send(())
            .await
            .expect("signal send failed");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            market_data
                .read()
                .await
                .gas_price()
                .is_none(),
            "gas price should remain None after failed fetch"
        );

        // Second signal → mock succeeds → gas price updated.
        trigger_and_wait(&signal_tx, &market_data, Duration::from_secs(2), |m| {
            m.gas_price().is_some()
        })
        .await;

        drop(signal_tx);
        let result = handle
            .await
            .expect("task should not panic");
        assert!(result.is_err(), "run() should return Err when signal channel closes");
    }

    #[tokio::test]
    async fn persistent_failure_keeps_loop_alive() {
        let market_data = MarketData::new_shared();
        let (mut fetcher, signal_tx) =
            GasPriceFetcher::new(MockFeePriceGetter::new(3), market_data.clone());

        let handle = tokio::spawn(async move { fetcher.run().await });

        // 3 failures — gas price remains None throughout.
        for _ in 0..3 {
            signal_tx
                .send(())
                .await
                .expect("signal send failed");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                market_data
                    .read()
                    .await
                    .gas_price()
                    .is_none(),
                "gas price should remain None during persistent failure"
            );
        }

        // 4th signal → mock succeeds → fetcher recovers.
        trigger_and_wait(&signal_tx, &market_data, Duration::from_secs(2), |m| {
            m.gas_price().is_some()
        })
        .await;

        drop(signal_tx);
        let _ = handle
            .await
            .expect("task should not panic");
    }
}
