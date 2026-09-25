use std::time::Duration;

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use zcash_client_backend::proto::service::{
    BlockId, ChainSpec, RawTransaction, TreeState,
    compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_protocol::consensus::BlockHeight;

use crate::Error;

pub type Lightwalletd = CompactTxStreamerClient<Channel>;

pub async fn connect(url: &str) -> Result<Lightwalletd, Error> {
    // Without keepalives a connection the server silently dropped would hang every call.
    let mut endpoint = Endpoint::from_shared(url.to_owned())
        .map_err(|e| Error::Config(format!("lightwalletd URL {url}: {e}")))?
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(20))
        .keep_alive_while_idle(true);
    if url.starts_with("https://") {
        endpoint = endpoint.tls_config(ClientTlsConfig::new().with_webpki_roots())?;
    }
    Ok(CompactTxStreamerClient::new(endpoint.connect().await?))
}

pub(crate) async fn chain_tip(client: &mut Lightwalletd) -> Result<BlockHeight, Error> {
    let block = client
        .get_latest_block(ChainSpec::default())
        .await?
        .into_inner();
    u32::try_from(block.height)
        .map(BlockHeight::from_u32)
        .map_err(|_| Error::Wallet(format!("implausible chain tip {}", block.height)))
}

pub(crate) async fn tree_state(
    client: &mut Lightwalletd,
    height: BlockHeight,
) -> Result<TreeState, Error> {
    let block = BlockId {
        height: u32::from(height).into(),
        hash: vec![],
    };
    Ok(client.get_tree_state(block).await?.into_inner())
}

pub(crate) async fn broadcast(client: &mut Lightwalletd, data: Vec<u8>) -> Result<(), Error> {
    let response = client
        .send_transaction(RawTransaction { data, height: 0 })
        .await?
        .into_inner();
    match response.error_code {
        0 => Ok(()),
        code => Err(Error::Rejected {
            code,
            message: response.error_message,
        }),
    }
}
