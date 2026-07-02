#![allow(
  dead_code,
  reason = "vendored unsync utilities; flow uses only a subset"
)]

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

#[repr(transparent)]
pub struct MaskValueAsSend<V> {
  pub value: V,
}

// SAFETY: the wrapped value is only ever constructed, polled, and dropped on
// the single thread that owns it (spawn_pinned-style usage); Send is masked
// solely to satisfy the spawn signature.
unsafe impl<R> Send for MaskValueAsSend<R> {}

impl<R> MaskValueAsSend<R> {
  #[inline(always)]
  pub fn into_inner(self) -> R {
    self.value
  }
}

pub struct MaskFutureAsSend<Fut> {
  pub fut: MaskValueAsSend<Fut>,
}

impl<Fut> From<Fut> for MaskFutureAsSend<Fut>
where
  Fut: Future,
{
  fn from(value: Fut) -> Self {
    Self {
      fut: MaskValueAsSend { value },
    }
  }
}

impl<Fut: Future> Future for MaskFutureAsSend<Fut> {
  type Output = Fut::Output;

  fn poll(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> Poll<Self::Output> {
    // SAFETY: structural pin projection; `fut.value` is never moved out of
    // the pinned wrapper.
    unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().fut.value) }
      .poll(cx)
  }
}
