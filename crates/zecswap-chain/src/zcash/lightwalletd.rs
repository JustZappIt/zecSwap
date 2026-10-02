use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use http_body::{Body, Frame};
use tokio::time::{Instant, Sleep};
use tonic::body::Body as TonicBody;
use tonic::codegen::{Bytes, http};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tower_service::Service;
use zcash_client_backend::proto::service::{
    BlockId, ChainSpec, RawTransaction, SendResponse, TreeState, TxFilter,
    compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_protocol::{TxId, consensus::BlockHeight};

use crate::Error;

const STREAM_IDLE: Duration = Duration::from_secs(30);
const RPC_TOTAL: Duration = Duration::from_secs(120);

pub type Lightwalletd = CompactTxStreamerClient<DeadlineChannel>;

/// Bounds the response body as well as the wait for headers, including streams opened by
/// the wallet backend. HTTP/2 keepalives alone do not detect a stalled RPC stream.
#[derive(Clone)]
pub struct DeadlineChannel(Channel);

impl From<Channel> for DeadlineChannel {
    fn from(channel: Channel) -> Self {
        Self(channel)
    }
}

impl Service<http::Request<TonicBody>> for DeadlineChannel {
    type Response = http::Response<TonicBody>;
    type Error = tonic::transport::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<TonicBody>) -> Self::Future {
        let deadline = Instant::now() + RPC_TOTAL;
        let response = self.0.call(request);
        Box::pin(async move {
            Ok(response
                .await?
                .map(|body| TonicBody::new(DeadlineBody::new(body, deadline))))
        })
    }
}

struct DeadlineBody {
    inner: TonicBody,
    idle: Pin<Box<Sleep>>,
    total: Pin<Box<Sleep>>,
    ended: bool,
}

impl DeadlineBody {
    fn new(inner: TonicBody, deadline: Instant) -> Self {
        Self {
            inner,
            idle: Box::pin(tokio::time::sleep(STREAM_IDLE)),
            total: Box::pin(tokio::time::sleep_until(deadline)),
            ended: false,
        }
    }
}

impl Body for DeadlineBody {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        if self.total.as_mut().poll(cx).is_ready() || self.idle.as_mut().poll(cx).is_ready() {
            self.ended = true;
            return Poll::Ready(Some(Err(tonic::Status::deadline_exceeded(
                "lightwalletd response deadline exceeded",
            ))));
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if frame.data_ref().is_some_and(|data| !data.is_empty()) {
                    self.idle.as_mut().reset(Instant::now() + STREAM_IDLE);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(result) => {
                self.ended = true;
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub async fn connect(url: &str) -> Result<Lightwalletd, Error> {
    Ok(CompactTxStreamerClient::new(
        endpoint(url)?.connect().await?.into(),
    ))
}

/// Allows an existing maker to start protecting EVM escrows during a Zcash outage.
pub fn connect_lazy(url: &str) -> Result<Lightwalletd, Error> {
    Ok(CompactTxStreamerClient::new(
        endpoint(url)?.connect_lazy().into(),
    ))
}

fn endpoint(url: &str) -> Result<Endpoint, Error> {
    // Without keepalives a connection the server silently dropped would hang every call.
    let mut endpoint = Endpoint::from_shared(url.to_owned())
        .map_err(|e| Error::Config(format!("lightwalletd URL {url}: {e}")))?
        .connect_timeout(Duration::from_secs(15))
        .timeout(RPC_TOTAL)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(20))
        .keep_alive_while_idle(true);
    if url.starts_with("https://") {
        endpoint = endpoint.tls_config(ClientTlsConfig::new().with_webpki_roots())?;
    }
    Ok(endpoint)
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

    struct TestBody(tokio::sync::mpsc::UnboundedReceiver<Result<Frame<Bytes>, tonic::Status>>);

    impl Body for TestBody {
        type Data = Bytes;
        type Error = tonic::Status;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            self.0.poll_recv(cx)
        }
    }

    async fn next_frame(body: &mut DeadlineBody) -> Option<Result<Frame<Bytes>, tonic::Status>> {
        std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
    }

    #[tokio::test(start_paused = true)]
    async fn response_headers_and_then_silence_hit_the_idle_deadline() {
        let (_sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let started = Instant::now();
        let mut body = DeadlineBody::new(TonicBody::new(TestBody(receiver)), started + RPC_TOTAL);
        let error = next_frame(&mut body).await.unwrap().unwrap_err();
        assert_eq!(error.code(), tonic::Code::DeadlineExceeded);
        assert_eq!(started.elapsed(), STREAM_IDLE);
        assert!(next_frame(&mut body).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn body_progress_resets_idle_but_cannot_extend_total_deadline() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let started = Instant::now();
        let mut body = DeadlineBody::new(TonicBody::new(TestBody(receiver)), started + RPC_TOTAL);
        for _ in 0..11 {
            tokio::time::advance(Duration::from_secs(10)).await;
            sender
                .send(Ok(Frame::data(Bytes::from_static(b"progress"))))
                .unwrap();
            assert!(next_frame(&mut body).await.unwrap().is_ok());
        }
        let error = next_frame(&mut body).await.unwrap().unwrap_err();
        assert_eq!(error.code(), tonic::Code::DeadlineExceeded);
        assert_eq!(started.elapsed(), RPC_TOTAL);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_stops_after_data_is_also_bounded() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut body = DeadlineBody::new(
            TonicBody::new(TestBody(receiver)),
            Instant::now() + RPC_TOTAL,
        );
        tokio::time::advance(Duration::from_secs(20)).await;
        sender
            .send(Ok(Frame::data(Bytes::from_static(b"block"))))
            .unwrap();
        assert!(next_frame(&mut body).await.unwrap().is_ok());
        let last_progress = Instant::now();
        assert_eq!(
            next_frame(&mut body).await.unwrap().unwrap_err().code(),
            tonic::Code::DeadlineExceeded
        );
        assert_eq!(last_progress.elapsed(), STREAM_IDLE);
    }

    #[tokio::test(start_paused = true)]
    async fn completed_bodies_pass_through_data_and_trailers() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut body = DeadlineBody::new(
            TonicBody::new(TestBody(receiver)),
            Instant::now() + RPC_TOTAL,
        );
        sender
            .send(Ok(Frame::data(Bytes::from_static(b"block"))))
            .unwrap();
        sender
            .send(Ok(Frame::trailers(http::HeaderMap::new())))
            .unwrap();
        drop(sender);
        assert_eq!(
            next_frame(&mut body)
                .await
                .unwrap()
                .unwrap()
                .data_ref()
                .unwrap(),
            b"block".as_slice()
        );
        assert!(next_frame(&mut body).await.unwrap().unwrap().is_trailers());
        assert!(next_frame(&mut body).await.is_none());
    }

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
