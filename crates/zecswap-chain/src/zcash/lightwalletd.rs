use std::{future::Future, time::Duration};

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use zcash_client_backend::proto::service::{
    BlockId, ChainSpec, RawTransaction, SendResponse, TreeState, TxFilter,
    compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_protocol::{TxId, consensus::BlockHeight};

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

pub(crate) async fn broadcast(
    client: &mut Lightwalletd,
    txid: TxId,
    data: Vec<u8>,
) -> Result<(), Error> {
    let response = client
        .send_transaction(RawTransaction {
            data: data.clone(),
            height: 0,
        })
        .await?
        .into_inner();
    confirm_submission(response, &data, async {
        client
            .get_transaction(TxFilter {
                hash: txid.as_ref().to_vec(),
                ..Default::default()
            })
            .await
            .map(|response| response.into_inner())
    })
    .await
}

async fn confirm_submission(
    response: SendResponse,
    data: &[u8],
    lookup: impl Future<Output = Result<RawTransaction, tonic::Status>>,
) -> Result<(), Error> {
    if response.error_code == 0 {
        return Ok(());
    }
    // Duplicate submissions differ by backend. Verify the exact transaction instead
    // of suppressing errors by matching strings or treating every rejection as success.
    if let Ok(Ok(known)) = tokio::time::timeout(Duration::from_secs(5), lookup).await
        && !data.is_empty()
        && known.data == data
        && known.height != u64::MAX
    {
        return Ok(());
    }
    Err(Error::Rejected {
        code: response.error_code,
        message: response.error_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(code: i32, message: &str) -> SendResponse {
        SendResponse {
            error_code: code,
            error_message: message.into(),
        }
    }

    #[tokio::test]
    async fn acknowledged_broadcast_needs_no_transaction_lookup() {
        assert!(
            confirm_submission(rejected(0, "txid"), &[1, 2, 3], async {
                panic!("successful broadcast performed a lookup");
            })
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn duplicate_or_already_mined_submissions_require_exact_server_confirmation() {
        for (code, message, height) in [
            (-1, "transaction already exists in mempool", 0),
            (-1, "transaction is already queued", 0),
            (-27, "transaction already in block chain", 4_431_986),
            (-1, "unrecognized backend duplicate message", 0),
        ] {
            assert!(
                confirm_submission(rejected(code, message), &[1, 2, 3], async {
                    Ok(RawTransaction {
                        data: vec![1, 2, 3],
                        height,
                    })
                })
                .await
                .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn unverified_rejections_preserve_the_original_error() {
        for known in [
            Err(tonic::Status::not_found("unknown transaction")),
            Err(tonic::Status::unavailable("server unavailable")),
            Ok(RawTransaction {
                data: vec![4, 5, 6],
                height: 0,
            }),
            Ok(RawTransaction {
                data: vec![1, 2, 3],
                height: u64::MAX,
            }),
        ] {
            let error = confirm_submission(
                rejected(-1, "transaction already exists in mempool"),
                &[1, 2, 3],
                async { known },
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, Error::Rejected { code: -1, message } if message == "transaction already exists in mempool")
            );
        }
        let error = confirm_submission(rejected(-26, "invalid proof"), &[1, 2, 3], async {
            Err(tonic::Status::not_found("unknown transaction"))
        })
        .await
        .unwrap_err();
        assert!(
            matches!(error, Error::Rejected { code: -26, message } if message == "invalid proof")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unavailable_confirmation_is_bounded_and_never_silences_the_error() {
        let result = tokio::time::timeout(
            Duration::from_secs(6),
            confirm_submission(
                rejected(-1, "transaction already exists in mempool"),
                &[1, 2, 3],
                std::future::pending(),
            ),
        )
        .await
        .expect("confirmation stalled beyond its timeout");
        assert!(matches!(result, Err(Error::Rejected { code: -1, .. })));
    }
}
