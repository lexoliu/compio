use std::{
    cell::UnsafeCell,
    fmt::Debug,
    io,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
    rc::{Rc, Weak},
    slice,
};

use compio_buf::{IoBuf, IoBufMut, SetLen};

use crate::{Extra, sys::BufControl};

/// Trait used to allocate buffers for compio-driver's buffer pool.
///
/// Default implementation is [`BoxAllocator`], which uses [`Box`] to allocate
/// and deallocate each buffer.
pub trait BufferAllocator {
    /// Allocate a chunk of memory with `len`.
    fn allocate(len: u32) -> NonNull<MaybeUninit<u8>>;

    /// Deallocate a chunk of memory.
    ///
    /// # Safety
    ///
    /// The pointer passed in must be previously allocated by this allocator.
    unsafe fn deallocate(ptr: NonNull<MaybeUninit<u8>>, len: u32);
}

/// Default implementation of [`BufferAllocator`].
pub struct BoxAllocator;

// Default implementation of [`BufferAllocator`]
impl BufferAllocator for BoxAllocator {
    fn allocate(len: u32) -> NonNull<MaybeUninit<u8>> {
        let ptr = Box::into_raw(Box::<[u8]>::new_uninit_slice(len as usize)).cast();
        // SAFETY: Creating `NonNull` from `Box`
        unsafe { NonNull::new_unchecked(ptr) }
    }

    unsafe fn deallocate(ptr: NonNull<MaybeUninit<u8>>, len: u32) {
        let ptr = ptr::slice_from_raw_parts_mut(ptr.as_ptr(), len as usize);
        // SAFETY: Caller guarantees the pointer was allocated by us with `len`
        _ = unsafe { Box::from_raw(ptr) };
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BufferAlloc {
    allocate: fn(len: u32) -> NonNull<MaybeUninit<u8>>,
    deallocate: unsafe fn(ptr: NonNull<MaybeUninit<u8>>, len: u32),
}

impl BufferAlloc {
    pub fn new<A: BufferAllocator>() -> Self {
        Self {
            allocate: A::allocate,
            deallocate: A::deallocate,
        }
    }
}

/// A buffer pointer without length part.
pub(crate) type BufPtr = NonNull<MaybeUninit<u8>>;

/// A buffer pool.
///
/// This type by itself does nothing, and should only be used by `*Managed` ops.
#[derive(Clone)]
pub struct BufferPool {
    shared: Weak<Shared>,
}

#[repr(transparent)]
#[derive(Debug)]
pub(crate) struct BufferPoolRoot {
    shared: Rc<Shared>,
}

/// A reference to the bytes of a buffer within the buffer pool.
///
/// Dropping this type will reset the buffer back to the pool instead of
/// releasing buffer's memory.
///
/// On a pool that consumes its buffers incrementally (see
/// [`BufferPool::is_incremental`]), several references can point into one
/// buffer, each to the part one completion filled. The buffer goes back to the
/// pool once the kernel has finished with it and the last of them is dropped.
pub struct BufferRef {
    /// Initialized length of the buffer, set with [`SetLen`]
    len: u32,
    /// User-set capacity, default to `full_cap`
    cap: u32,
    /// Full capacity of this reference: the whole buffer, or the part of it
    /// one completion filled on an incremental pool
    full_cap: u32,
    /// The buffer pool, kept alive until the last reference into it is
    /// dropped, so that a buffer still referenced when the driver is dropped
    /// is deallocated then
    shared: Rc<Shared>,
    /// Pointer to the first byte of this reference
    ptr: BufPtr,
    /// Buffer id (index within the Vec)
    buffer_id: u16,
}

/// What a completion did with a buffer of the pool.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Filled {
    buffer_id: u16,
    /// Bytes the completion wrote into it
    len: u32,
    /// The kernel keeps the buffer for more completions
    /// (`IORING_CQE_F_BUF_MORE`)
    more: bool,
}

impl Filled {
    /// The buffer of a completion that wrote `len` bytes, described by
    /// `extra`.
    pub(crate) fn new(extra: &Extra, len: usize) -> io::Result<Self> {
        Ok(Self {
            buffer_id: extra.buffer_id()?,
            // A completion's result is an `i32`.
            len: len as u32,
            more: extra.buffer_more()?,
        })
    }
}

#[repr(transparent)]
struct Shared {
    inner: UnsafeCell<Inner>,
}

struct Inner {
    /// Allocator of the buffers
    alloc: BufferAlloc,

    /// Control block corresponds to each driver
    ctrl: BufControl,

    /// Size of each buffer
    size: u32,

    /// Buffers, indexed by buffer id
    bufs: Vec<Buffer>,

    /// The control block is released: a buffer still referenced is
    /// deallocated when its last reference is dropped
    released: bool,
}

struct Buffer {
    ptr: BufPtr,
    /// The buffer is in the control block's ring: free for the kernel to fill,
    /// or, on an incremental pool, partly filled and kept by the kernel for
    /// more completions
    in_ring: bool,
    /// Bytes filled since the buffer was last put in the ring: where the next
    /// completion into it begins on an incremental pool
    filled: u32,
    /// Live [`BufferRef`]s into the buffer
    refs: u32,
}

impl BufferPoolRoot {
    pub(crate) fn new(
        driver: &mut crate::Driver,
        alloc: BufferAlloc,
        num_of_bufs: u16,
        buffer_size: usize,
        flags: u16,
    ) -> io::Result<Self> {
        let size: u32 = buffer_size.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Buffer size too large. Should be able to fit into u32.",
            )
        })?;
        let ptrs = (0..num_of_bufs.next_power_of_two())
            .map(|_| (alloc.allocate)(size))
            .collect::<Vec<_>>();
        let ctrl = match unsafe { BufControl::new(driver, &ptrs, size, flags) } {
            Ok(ctrl) => ctrl,
            Err(e) => {
                for ptr in ptrs {
                    // SAFETY: The control block was not created, so nothing
                    // else refers to the buffers
                    unsafe { (alloc.deallocate)(ptr, size) }
                }
                return Err(e);
            }
        };
        let bufs = ptrs
            .into_iter()
            .map(|ptr| Buffer {
                ptr,
                in_ring: true,
                filled: 0,
                refs: 0,
            })
            .collect();

        Ok(Self {
            shared: Shared {
                inner: Inner {
                    alloc,
                    ctrl,
                    size,
                    bufs,
                    released: false,
                }
                .into(),
            }
            .into(),
        })
    }

    /// Release the buffer pool and deallocate all buffers.
    ///
    /// If the buffer pool root is dropped without calling this function,
    /// everything will be leaked and there will be no chance to recover them
    /// back, except those referenced by [`BufferRef`], which will be released
    /// when the last reference to each is dropped.
    ///
    /// If the control block failed to release, this function will return an io
    /// Error without deallocating buffers, and it's possible to retry.
    ///
    /// # Safety
    ///
    /// [`BufferPoolRoot`] must not be used after `release` is called and
    /// returned successfully. Only thing that's safe to do afterwards is to
    /// drop it.
    pub(crate) unsafe fn release(&mut self, driver: &mut crate::Driver) -> io::Result<()> {
        unsafe {
            self.shared.with(|inner| {
                inner.ctrl.release(driver)?;
                // Control is successfully released, now deallocate the buffers
                // nothing refers to
                inner.released = true;
                for buf in &mut inner.bufs {
                    buf.in_ring = false;
                    if buf.refs == 0 {
                        (inner.alloc.deallocate)(buf.ptr, inner.size)
                    }
                }
                io::Result::Ok(())
            })
        }?;

        Ok(())
    }

    pub(crate) fn get_pool(&self) -> BufferPool {
        BufferPool {
            shared: Rc::downgrade(&self.shared),
        }
    }
}

impl Debug for BufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(shared) = self.shared.upgrade() {
            f.debug_struct("BufferPool")
                .field("shared", &shared)
                .finish()
        } else {
            f.debug_struct("BufferPool")
                .field("shared", &"<dropped>")
                .finish()
        }
    }
}

impl Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buf")
            .field("ptr", &self.ptr)
            .field("in_ring", &self.in_ring)
            .field("filled", &self.filled)
            .field("refs", &self.refs)
            .finish()
    }
}

impl Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe {
            self.with(|inner| {
                f.debug_struct("Shared")
                    .field("control", &inner.ctrl)
                    .field("size", &inner.size)
                    .field("buffers", &inner.bufs)
                    .field("released", &inner.released)
                    .finish()
            })
        }
    }
}

impl BufferPool {
    /// Pop an available buffer from the pool with given capacity.
    ///
    /// This operation is not supported on io-uring driver and will always
    /// return [`Unsupported`].
    ///
    /// [`Unsupported`]: io::ErrorKind::Unsupported
    pub fn pop(&self) -> io::Result<BufferRef> {
        let shared = self.shared()?;
        let buffer_id = unsafe { shared.with(|inner| inner.ctrl.pop()) }?;

        Ok(Shared::lend(&shared, buffer_id, None).expect("Buffer should be available"))
    }

    /// Take the buffer a completion wrote `len` bytes into, which `extra`
    /// identifies.
    ///
    /// The returned buffer holds those `len` bytes. On a pool that consumes its
    /// buffers incrementally (see [`is_incremental`]), it refers only to the
    /// part of the buffer this completion filled, and the buffer goes back to
    /// the pool once the kernel has finished with it and every reference into
    /// it is dropped.
    ///
    /// A multishot op lends the buffer of each completion out when the driver
    /// hands the completion to it, in the order the kernel posted them,
    /// because on an incremental pool a completion's bytes follow those of the
    /// completion before it in the same buffer, whichever op that was. `extra`
    /// then carries that buffer, and this returns it.
    ///
    /// Returns `None` if the buffer is not in the pool's ring (it was not
    /// reset back yet), or does not exist.
    ///
    /// [`is_incremental`]: Self::is_incremental
    pub fn take(&self, extra: &mut Extra, len: usize) -> io::Result<Option<BufferRef>> {
        #[cfg(io_uring)]
        if let Some(buffer) = extra.take_lent_buffer() {
            return Ok(Some(buffer));
        }
        self.take_filled(Filled::new(extra, len)?)
    }

    pub(crate) fn take_filled(&self, filled: Filled) -> io::Result<Option<BufferRef>> {
        let shared = self.shared()?;
        Ok(Shared::lend(&shared, filled.buffer_id, Some(filled)))
    }

    /// Reset the buffer a completion wrote `len` bytes into, which `extra`
    /// identifies, so that it's available for kernel to use once the kernel
    /// has finished with it. Returns whether such a buffer was in the ring.
    ///
    /// This is the same as `take(extra, len)` and immediately drop it.
    pub fn reset(&self, extra: &mut Extra, len: usize) -> io::Result<bool> {
        Ok(self.take(extra, len)?.is_some())
    }

    /// Test if the pool consumes its buffers incrementally.
    ///
    /// This is the case for a pool on io_uring whose flags (see
    /// [`ProactorBuilder::buffer_pool_flag`]) include `IOU_PBUF_RING_INC` and
    /// whose kernel accepted it (Linux 6.12). A completion then fills only
    /// the part of a buffer it needs, and the next one continues in it.
    ///
    /// [`ProactorBuilder::buffer_pool_flag`]: crate::ProactorBuilder::buffer_pool_flag
    pub fn is_incremental(&self) -> io::Result<bool> {
        unsafe { self.with(|inner| inner.ctrl.is_incremental()) }
    }

    fn shared(&self) -> io::Result<Rc<Shared>> {
        self.shared
            .upgrade()
            // A buffer referenced past the driver's drop keeps the pool alive
            .filter(|shared| unsafe { !shared.with(|inner| inner.released) })
            .ok_or_else(|| io::Error::other("The driver has been dropped"))
    }

    /// # Safety
    ///
    /// `f` must not access `self` reentrantly
    unsafe fn with<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut Inner) -> R,
    {
        Ok(unsafe { self.shared()?.with(f) })
    }

    /// Get the group id of this buffer pool.
    #[cfg(io_uring)]
    pub(crate) fn buffer_group(&self) -> io::Result<u16> {
        unsafe { self.with(|i| i.ctrl.buffer_group()) }
    }

    /// Test if the buffer pool is an io_uring one.
    #[cfg(fusion)]
    pub fn is_io_uring(&self) -> io::Result<bool> {
        unsafe { self.with(|inner| inner.ctrl.is_io_uring()) }
    }
}

impl Shared {
    /// # Safety
    ///
    /// `f` must not access [`Self::inner`] reentrantly
    #[inline(always)]
    unsafe fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Inner) -> R,
    {
        f(unsafe { &mut *self.inner.get() })
    }

    /// Lend buffer `buffer_id` out of the ring: the part `filled` wrote on an
    /// incremental pool, the whole buffer otherwise.
    fn lend(this: &Rc<Self>, buffer_id: u16, filled: Option<Filled>) -> Option<BufferRef> {
        let (ptr, cap, len) = unsafe { this.with(|inner| inner.lend(buffer_id, filled)) }?;
        Some(BufferRef {
            len,
            cap,
            full_cap: cap,
            shared: this.clone(),
            ptr,
            buffer_id,
        })
    }
}

impl Inner {
    /// Returns where the reference begins, its capacity and its initialized
    /// length.
    fn lend(&mut self, buffer_id: u16, filled: Option<Filled>) -> Option<(BufPtr, u32, u32)> {
        let incremental = self.ctrl.is_incremental();
        let size = self.size;
        let buf = self.bufs.get_mut(buffer_id as usize)?;
        if !buf.in_ring {
            return None;
        }
        buf.refs += 1;
        match filled {
            Some(Filled { len, more, .. }) if incremental => {
                let start = buf.filled;
                debug_assert!(
                    len <= size - start,
                    "a completion filled past the end of buffer {buffer_id}"
                );
                buf.filled += len;
                // The kernel gives a buffer up when a completion fills it to
                // the end, and posts that completion without
                // `IORING_CQE_F_BUF_MORE`. A completion that wrote nothing
                // leaves the buffer in the ring whatever its flags say
                // (`io_kbuf_inc_commit` in io_uring/kbuf.c).
                if !more && len > 0 {
                    buf.in_ring = false;
                }
                // SAFETY: `start + len` is within the buffer
                Some((unsafe { buf.ptr.add(start as usize) }, len, len))
            }
            filled => {
                buf.in_ring = false;
                Some((buf.ptr, size, filled.map_or(0, |filled| filled.len)))
            }
        }
    }

    /// Drop a reference into buffer `buffer_id`, giving the buffer back to the
    /// ring once the kernel is done with it and nothing refers to it.
    fn release_ref(&mut self, buffer_id: u16) {
        let buf = &mut self.bufs[buffer_id as usize];
        buf.refs -= 1;
        if buf.refs > 0 || buf.in_ring {
            return;
        }
        if self.released {
            // SAFETY: The control block is released and nothing refers to the
            // buffer
            unsafe { (self.alloc.deallocate)(buf.ptr, self.size) }
        } else {
            buf.in_ring = true;
            buf.filled = 0;
            // SAFETY: The buffer lives until the pool is released
            unsafe { self.ctrl.reset(buffer_id, buf.ptr, self.size) };
        }
    }
}

impl BufferRef {
    /// Set the capacity of this buffer.
    ///
    /// This does nothing if `cap` is greater than underlying buffer's
    /// length.
    pub fn with_capacity(mut self, cap: usize) -> Self {
        self.set_capacity(cap);
        self
    }

    /// Set the capacity of this buffer.
    ///
    /// This does nothing if `cap` is greater than underlying buffer's
    /// length or equals 0.
    pub fn set_capacity(&mut self, cap: usize) {
        if cap == 0 {
            return;
        }
        self.cap = (cap as u32).min(self.full_cap);
        self.len = self.len.min(self.cap);
    }
}

impl Debug for BufferRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferRef")
            .field("buffer_id", &self.buffer_id)
            .field("ptr", &self.ptr)
            .field("len", &self.len)
            .field("cap", &self.cap)
            .field("full_cap", &self.full_cap)
            .finish_non_exhaustive()
    }
}

impl Deref for BufferRef {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: `SetLen` guarantees the range is initialized
        unsafe { slice::from_raw_parts(self.ptr.as_ptr().cast(), self.len as usize) }
    }
}

impl DerefMut for BufferRef {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `SetLen` guarantees the range is initialized
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr() as _, self.len as usize) }
    }
}

impl IoBuf for BufferRef {
    fn as_init(&self) -> &[u8] {
        self
    }
}

impl SetLen for BufferRef {
    unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= u32::MAX as usize);
        self.len = (len as u32).min(self.cap);
    }
}

impl IoBufMut for BufferRef {
    unsafe fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        // SAFETY: Cap is initialized as the length of this reference, and
        // setting it is capped at full_cap, so it will never exceed it, nor
        // reach into another reference to the same buffer. Pointer is not
        // deallocated.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap as usize) }
    }
}

impl Drop for BufferRef {
    fn drop(&mut self) {
        unsafe { self.shared.with(|inner| inner.release_ref(self.buffer_id)) }
    }
}
