use std::borrow::Cow;
use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Poll;

use anyhow::Context;
use deno_core::ByteString;
use deno_core::OpState;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::op2;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::ready;
use log::error;
use serde::Serialize;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::DuplexStream;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

deno_core::extension!(
  runtime_http,
  ops = [
    op_http_upgrade_websocket2,
    op_http_upgrade_raw2,
    op_http_upgrade_raw2_fence
  ],
  middleware = |op| match op.name {
    "op_http_upgrade_websocket" => {
      op.with_implementation_from(&op_http_upgrade_websocket2())
    }
    _ => op,
  },
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
  Normal,
  Dropping,
  Dropped,
}

pub(crate) struct Stream2<S>
where
  S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
{
  io: Option<(S, Option<CancellationToken>)>,
  state: StreamState,
  wait_fut: Option<BoxFuture<'static, ()>>,
}

impl<S> Drop for Stream2<S>
where
  S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
{
  fn drop(&mut self) {
    if self.state != StreamState::Normal {
      return;
    }

    let Some((stream, conn_sync)) = self.io.take() else {
      return;
    };

    let mut stream = Stream2::new(stream, conn_sync);

    stream.state = StreamState::Dropping;

    // TODO(Nyannyacha): Optimize this. No matter how I think about it,
    // using `tokio::spawn` to defer the stream shutdown seems like a waste.
    drop(tokio::spawn(async move {
      match stream.shutdown().await {
        Ok(_) => {}
        Err(e) => {
          error!("stream could not be shutdown properly: {}", e);
        }
      }
    }));
  }
}

impl<S> Stream2<S>
where
  S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
{
  pub fn new(stream: S, token: Option<CancellationToken>) -> Self {
    Self {
      io: Some((stream, token)),
      state: StreamState::Normal,
      wait_fut: None,
    }
  }

  pub fn is_dropped(&self) -> bool {
    self.state == StreamState::Dropped
  }

  #[allow(
    dead_code,
    reason = "used by the main<->user-worker request-passing upgrade ops, which are currently stubbed out (comms redesign pending)"
  )]
  fn into_inner(mut self) -> Option<(S, Option<CancellationToken>)> {
    self.io.take()
  }
}

impl<S> AsyncRead for Stream2<S>
where
  S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
{
  fn poll_read(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
    buf: &mut tokio::io::ReadBuf<'_>,
  ) -> Poll<std::io::Result<()>> {
    if let Some((stream, _)) = Pin::into_inner(self).io.as_mut() {
      Pin::new(stream).poll_read(cx, buf)
    } else {
      Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
    }
  }
}

impl<S> AsyncWrite for Stream2<S>
where
  S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
{
  fn poll_write(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
    buf: &[u8],
  ) -> Poll<Result<usize, std::io::Error>> {
    if let Some((stream, _)) = Pin::into_inner(self).io.as_mut() {
      Pin::new(stream).poll_write(cx, buf)
    } else {
      Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
    }
  }

  fn poll_write_vectored(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
    bufs: &[std::io::IoSlice<'_>],
  ) -> Poll<Result<usize, std::io::Error>> {
    if let Some((stream, _)) = Pin::into_inner(self).io.as_mut() {
      Pin::new(stream).poll_write_vectored(cx, bufs)
    } else {
      Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
    }
  }

  fn is_write_vectored(&self) -> bool {
    self
      .io
      .as_ref()
      .map(|(it, _)| it.is_write_vectored())
      .unwrap_or_default()
  }

  fn poll_flush(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> Poll<Result<(), std::io::Error>> {
    if let Some((stream, _)) = Pin::into_inner(self).io.as_mut() {
      Pin::new(stream).poll_flush(cx)
    } else {
      Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
    }
  }

  fn poll_shutdown(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> Poll<Result<(), std::io::Error>> {
    let this = Pin::into_inner(self);

    if this.is_dropped() {
      return Poll::Ready(Ok(()));
    }

    if let Some((stream, token)) = this.io.as_mut() {
      if let Some(token) = token {
        let fut = this
          .wait_fut
          .get_or_insert_with(|| token.clone().cancelled_owned().boxed());

        ready!(fut.as_mut().poll_unpin(cx));
      }

      let poll_result = ready!(Pin::new(stream).poll_shutdown(cx));

      this.state = StreamState::Dropped;

      Poll::Ready(poll_result)
    } else {
      Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
    }
  }
}

pub(crate) type DuplexStream2 = Stream2<DuplexStream>;
#[allow(
  dead_code,
  reason = "used by the stubbed request-passing upgrade ops (comms redesign pending)"
)]
pub(crate) type UnixStream2 = Stream2<UnixStream>;

fn http_error(message: &'static str) -> crate::RuntimeError {
  crate::RuntimeError::Http(message.to_string())
}

#[allow(
  clippy::unused_async,
  reason = "must stay an async op to keep the contract of op_http_upgrade_websocket, which it replaces via extension middleware"
)]
#[op2]
#[smi]
async fn op_http_upgrade_websocket2(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<ResourceId, crate::RuntimeError> {
  // main<->user-worker request passing is disabled pending the comms redesign.
  let _ = (state, rid);
  Err(http_error(
    "user-worker request passing is not available (comms redesign pending)",
  ))
}

#[op2]
#[serde]
fn op_http_upgrade_raw2(
  state: &mut OpState,
  #[smi] stream_rid: ResourceId,
) -> Result<(ResourceId, ResourceId), crate::RuntimeError> {
  // main<->user-worker request passing is disabled pending the comms redesign.
  let _ = (state, stream_rid);
  Err(http_error(
    "user-worker request passing is not available (comms redesign pending)",
  ))
}

#[op2]
#[serde]
async fn op_http_upgrade_raw2_fence(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<HttpUpgradeRawResponseResource, crate::RuntimeError> {
  let resp = state
    .borrow_mut()
    .resource_table
    .take::<HttpUpgradeRawResponseFenceResource>(rid)?;

  Ok(HttpUpgradeRawResponseResource::new(
    Rc::into_inner(resp)
      .unwrap()
      .0
      .await
      .with_context(|| "cannot receive response")?,
  ))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HttpUpgradeRawResponseResource {
  status: u16,
  headers: Vec<(ByteString, ByteString)>,
}

impl Resource for HttpUpgradeRawResponseResource {
  fn name(&self) -> Cow<'_, str> {
    "httpUpgradeRawResponseResource".into()
  }
}

impl HttpUpgradeRawResponseResource {
  fn new(res: http::Response<()>) -> Self {
    let status = res.status().as_u16();
    let mut headers = vec![];

    for (key, value) in res.headers().iter() {
      headers.push((
        ByteString::from(key.as_str()),
        ByteString::from(value.to_str().unwrap_or_default()),
      ));
    }

    Self { status, headers }
  }
}

struct HttpUpgradeRawResponseFenceResource(
  oneshot::Receiver<http::Response<()>>,
);

impl Resource for HttpUpgradeRawResponseFenceResource {
  fn name(&self) -> Cow<'_, str> {
    "httpUpgradeRawResponseFenceResource".into()
  }
}
