//! Operations on registered (fixed) buffers.
//!
//! A buffer registered with [`Proactor::register_buffers`] is pinned once, at
//! registration; an operation that names it by its index skips pinning and
//! unpinning its pages, and a zerocopy send from it also skips charging them
//! to `RLIMIT_MEMLOCK`.
//!
//! The operation's buffer must lie entirely inside the registered buffer the
//! index names, or the kernel fails the operation with `EFAULT`. These
//! operations exist only with io-uring; under the fused driver they fail with
//! [`Unsupported`] when the runtime fell back to polling, where nothing can be
//! registered.
//!
//! [`Proactor::register_buffers`]: crate::Proactor::register_buffers
//! [`Unsupported`]: std::io::ErrorKind::Unsupported

use io_uring::{opcode, types::Fd};
use rustix::net::SendFlags;

use crate::{IourOpCode, OpEntry, sys::op::*};

/// Zerocopy [`Send`] from a registered buffer.
pub struct SendZcFixed<T: IoBuf, S> {
    op: Send<T, S>,
    buf_index: u16,
    res: Option<BufResult<usize, crate::Extra>>,
}

impl<T: IoBuf, S> SendZcFixed<T, S> {
    /// Create [`SendZcFixed`], sending `buffer`, which lies inside the
    /// registered buffer `buf_index`.
    pub fn new(fd: S, buffer: T, buf_index: u16, flags: SendFlags) -> Self {
        Self {
            op: Send::new(fd, buffer, flags),
            buf_index,
            res: None,
        }
    }
}

impl<T: IoBuf, S> IntoInner for SendZcFixed<T, S> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.op.into_inner()
    }
}

unsafe impl<T: IoBuf, S: AsFd> IourOpCode for SendZcFixed<T, S> {
    type Control = ();

    fn create_entry(&mut self, _: &mut Self::Control) -> OpEntry {
        let slice = self.op.buffer.as_init();
        opcode::SendZc::new(
            Fd(self.op.fd.as_fd().as_raw_fd()),
            slice.as_ptr(),
            slice.len().try_into().unwrap_or(u32::MAX),
        )
        .buf_index(Some(self.buf_index))
        .flags(self.op.flags.bits() as _)
        .build()
        .into()
    }

    fn create_entry_fallback(&mut self, control: &mut Self::Control) -> OpEntry {
        IourOpCode::create_entry(&mut self.op, control)
    }

    unsafe fn push_multishot(
        &mut self,
        _: &mut Self::Control,
        res: io::Result<usize>,
        extra: crate::Extra,
    ) {
        self.res.replace(BufResult(res, extra));
    }

    fn pop_multishot(&mut self, _: &mut Self::Control) -> Option<BufResult<usize, crate::Extra>> {
        self.res.take()
    }
}

/// Zerocopy [`SendVectored`] from a registered buffer: every slice lies
/// inside the one registered buffer `buf_index`.
///
/// Kernels before 6.15 reject the operation with `EINVAL` before sending
/// anything.
pub struct SendVectoredZcFixed<T: IoVectoredBuf, S> {
    op: SendVectored<T, S>,
    buf_index: u16,
    res: Option<BufResult<usize, crate::Extra>>,
}

impl<T: IoVectoredBuf, S> SendVectoredZcFixed<T, S> {
    /// Create [`SendVectoredZcFixed`], sending `buffer`, whose every slice
    /// lies inside the registered buffer `buf_index`.
    pub fn new(fd: S, buffer: T, buf_index: u16, flags: SendFlags) -> Self {
        Self {
            op: SendVectored::new(fd, buffer, flags),
            buf_index,
            res: None,
        }
    }
}

impl<T: IoVectoredBuf, S> IntoInner for SendVectoredZcFixed<T, S> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.op.into_inner()
    }
}

unsafe impl<T: IoVectoredBuf, S: AsFd> IourOpCode for SendVectoredZcFixed<T, S> {
    type Control = SendVectoredControl;

    unsafe fn init(&mut self, ctrl: &mut Self::Control) {
        self.op.init_control(ctrl)
    }

    fn create_entry(&mut self, control: &mut Self::Control) -> OpEntry {
        opcode::SendMsgZc::new(Fd(self.op.fd.as_fd().as_raw_fd()), &control.msg)
            .buf_index(Some(self.buf_index))
            .flags(self.op.flags.bits() as _)
            .build()
            .into()
    }

    fn create_entry_fallback(&mut self, control: &mut Self::Control) -> OpEntry {
        IourOpCode::create_entry(&mut self.op, control)
    }

    unsafe fn push_multishot(
        &mut self,
        _: &mut Self::Control,
        res: io::Result<usize>,
        extra: crate::Extra,
    ) {
        self.res.replace(BufResult(res, extra));
    }

    fn pop_multishot(&mut self, _: &mut Self::Control) -> Option<BufResult<usize, crate::Extra>> {
        self.res.take()
    }
}

/// [`ReadAt`] into a registered buffer.
pub struct ReadFixedAt<T: IoBufMut, S> {
    op: ReadAt<T, S>,
    buf_index: u16,
}

impl<T: IoBufMut, S> ReadFixedAt<T, S> {
    /// Create [`ReadFixedAt`], reading into `buffer`, which lies inside the
    /// registered buffer `buf_index`.
    pub fn new(fd: S, offset: u64, buffer: T, buf_index: u16) -> Self {
        Self {
            op: ReadAt::new(fd, offset, buffer),
            buf_index,
        }
    }
}

impl<T: IoBufMut, S> IntoInner for ReadFixedAt<T, S> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.op.into_inner()
    }
}

unsafe impl<T: IoBufMut, S: AsFd> IourOpCode for ReadFixedAt<T, S> {
    type Control = ();

    fn create_entry(&mut self, _: &mut Self::Control) -> OpEntry {
        let slice = self.op.buffer.sys_slice_mut();
        opcode::ReadFixed::new(
            Fd(self.op.fd.as_fd().as_raw_fd()),
            slice.ptr() as _,
            slice.len().try_into().unwrap_or(u32::MAX),
            self.buf_index,
        )
        .offset(self.op.offset)
        .build()
        .into()
    }

    fn create_entry_fallback(&mut self, control: &mut Self::Control) -> OpEntry {
        IourOpCode::create_entry(&mut self.op, control)
    }
}

#[cfg(fusion)]
mod poll {
    use super::*;
    use crate::{Decision, PollOpCode};

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "registered buffers are only supported on io-uring driver",
        )
    }

    macro_rules! unsupported_on_poll {
        ($(<$($g:ident: $b:path),*> $t:ty;)*) => {$(
            // SAFETY: the operation fails before anything is submitted.
            unsafe impl<$($g: $b),*> PollOpCode for $t {
                type Control = ();

                fn pre_submit(&mut self, _: &mut ()) -> io::Result<Decision> {
                    Err(unsupported())
                }

                fn operate(&mut self, _: &mut ()) -> Poll<io::Result<usize>> {
                    Poll::Ready(Err(unsupported()))
                }
            }
        )*};
    }

    unsupported_on_poll! {
        <T: IoBuf, S: AsFd> SendZcFixed<T, S>;
        <T: IoVectoredBuf, S: AsFd> SendVectoredZcFixed<T, S>;
        <T: IoBufMut, S: AsFd> ReadFixedAt<T, S>;
    }
}
