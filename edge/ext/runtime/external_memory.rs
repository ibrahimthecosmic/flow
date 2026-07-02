use std::ffi::c_void;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use deno_core::v8;
use deno_core::v8::UniqueRef;
use futures::task::AtomicWaker;

pub struct CustomAllocator {
  max: usize,
  count: AtomicUsize,
  waker: RwLock<Option<Arc<AtomicWaker>>>,
}

impl CustomAllocator {
  pub fn new(max: usize) -> Arc<Self> {
    Arc::new(Self {
      max,
      count: AtomicUsize::new(0),
      waker: RwLock::new(None),
    })
  }

  pub fn set_waker(&self, waker: Arc<AtomicWaker>) {
    _ = self.waker.try_write().unwrap().insert(waker);
  }

  pub fn into_v8_allocator(
    self: Arc<Self>,
  ) -> UniqueRef<deno_core::v8::Allocator> {
    let vtable: &'static v8::RustAllocatorVtable<CustomAllocator> =
      &v8::RustAllocatorVtable {
        allocate,
        allocate_uninitialized,
        free,
        drop,
      };

    // SAFETY: the vtable functions uphold the v8::Allocator contract, and the
    // strong count leaked by Arc::into_raw is reclaimed in `drop` below.
    unsafe { v8::new_rust_allocator(Arc::into_raw(self), vtable) }
  }

  fn wake(&self) {
    if let Some(waker) = self.waker.try_read().ok().and_then(|it| it.clone()) {
      waker.wake();
    }
  }
}

unsafe extern "C" fn allocate(
  allocator: &CustomAllocator,
  n: usize,
) -> *mut c_void {
  allocator.count.fetch_add(n, Ordering::SeqCst);

  let count_loaded = allocator.count.load(Ordering::SeqCst);

  if count_loaded > allocator.max {
    return std::ptr::null_mut();
  }

  allocator.wake();

  Box::into_raw(vec![0u8; n].into_boxed_slice()) as *mut c_void
}

#[allow(
  clippy::uninit_vec,
  reason = "allocate_uninitialized hands the buffer to V8 as ArrayBuffer backing storage, which does not require initialized memory"
)]
unsafe extern "C" fn allocate_uninitialized(
  allocator: &CustomAllocator,
  n: usize,
) -> *mut c_void {
  allocator.count.fetch_add(n, Ordering::SeqCst);

  let count_loaded = allocator.count.load(Ordering::SeqCst);

  if count_loaded > allocator.max {
    return std::ptr::null_mut();
  }

  let mut store: Vec<u8> = Vec::with_capacity(n);

  // SAFETY: the capacity was just reserved above; the elements are u8, for
  // which uninitialized contents are acceptable to V8 (see allow above).
  unsafe {
    store.set_len(n);
  }
  allocator.wake();

  Box::into_raw(store.into_boxed_slice()) as *mut c_void
}

unsafe extern "C" fn free(
  allocator: &CustomAllocator,
  data: *mut c_void,
  n: usize,
) {
  allocator.count.fetch_sub(n, Ordering::SeqCst);
  allocator.wake();

  // SAFETY: V8 frees exactly what `allocate`/`allocate_uninitialized`
  // returned, so (data, n) reconstructs the boxed slice created there.
  unsafe {
    let _ =
      Box::from_raw(std::ptr::slice_from_raw_parts_mut(data as *mut u8, n));
  }
}

unsafe extern "C" fn drop(allocator: *const CustomAllocator) {
  // SAFETY: releases the strong count leaked by Arc::into_raw in
  // `into_v8_allocator`; V8 calls this exactly once.
  unsafe {
    Arc::from_raw(allocator);
  }
}
