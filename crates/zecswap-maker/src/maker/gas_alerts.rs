use super::Maker;

impl Maker {
    /// With `[gas_alerts]` and Telegram, watches the ETH of the maker's own account.
    pub(super) async fn run_gas_alerts(&self) {
        let Some(alerts) = self
            .config
            .gas_alerts
            .as_ref()
            .filter(|_| self.telegram.enabled())
        else {
            return std::future::pending().await;
        };
        zecswap_alerts::gas::watch(
            self.store.outbox(),
            &self.settlement,
            alerts,
            "Maker",
            self.alert_network(),
            self.chain_id,
        )
        .await;
    }
}
