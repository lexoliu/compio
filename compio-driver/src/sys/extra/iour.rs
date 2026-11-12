use io_uring::squeue::Flags;

/// Extra data for RawOp.
#[derive(Debug)]
pub(in crate::sys) struct Extra {
    sqe_flags: Flags,
    cqe_flags: u32,
    /// The second half of a 32-byte CQE.
    #[cfg(feature = "io-uring-cqe32")]
    big_cqe: [u64; 2],
    personality: Option<u16>,
    /// The buffer of the pool a multishot completion filled, lent out when the
    /// driver handed the completion to its op, until it is taken with
    /// [`BufferPool::take`](crate::BufferPool::take).
    lent_buffer: Option<crate::BufferRef>,
}

pub(in crate::sys) use Extra as IourExtra;

impl Extra {
    pub fn new() -> Self {
        Self {
            sqe_flags: Flags::empty(),
            cqe_flags: 0,
            #[cfg(feature = "io-uring-cqe32")]
            big_cqe: [0; 2],
            personality: None,
            lent_buffer: None,
        }
    }

    pub fn set_personality(&mut self, personality: u16) {
        self.personality = Some(personality);
    }

    pub fn set_link(&mut self) {
        self.sqe_flags |= Flags::IO_LINK;
    }

    pub fn set_hardlink(&mut self) {
        self.sqe_flags |= Flags::IO_HARDLINK;
    }

    pub fn set_drain(&mut self) {
        self.sqe_flags |= Flags::IO_DRAIN;
    }

    pub fn get_personality(&self) -> Option<u16> {
        self.personality
    }

    pub fn get_sqe_flags(&self) -> Flags {
        self.sqe_flags
    }

    pub fn buffer_id(&self) -> Option<u16> {
        io_uring::cqueue::buffer_select(self.cqe_flags)
    }

    pub fn buffer_more(&self) -> bool {
        io_uring::cqueue::buffer_more(self.cqe_flags)
    }

    pub fn sock_nonempty(&self) -> bool {
        io_uring::cqueue::sock_nonempty(self.cqe_flags)
    }

    pub fn is_notification(&self) -> bool {
        io_uring::cqueue::notif(self.cqe_flags)
    }

    #[cfg(feature = "io-uring-cqe32")]
    pub fn big_cqe(&self) -> Option<[u64; 2]> {
        Some(self.big_cqe)
    }

    #[cfg(not(feature = "io-uring-cqe32"))]
    pub fn big_cqe(&self) -> Option<[u64; 2]> {
        None
    }
}

#[allow(dead_code)]
#[cfg(not(fusion))]
impl crate::sys::Extra {
    pub(crate) fn is_iour(&self) -> bool {
        true
    }

    pub(in crate::sys) fn try_as_iour(&self) -> Option<&Extra> {
        Some(&self.0)
    }

    pub(in crate::sys) fn try_as_iour_mut(&mut self) -> Option<&mut Extra> {
        Some(&mut self.0)
    }
}

#[allow(dead_code)]
impl crate::sys::Extra {
    pub(in crate::sys) fn as_iour(&self) -> &Extra {
        self.try_as_iour()
            .expect("Current driver is not `io_uring`")
    }

    pub(in crate::sys) fn as_iour_mut(&mut self) -> &mut Extra {
        self.try_as_iour_mut()
            .expect("Current driver is not `io_uring`")
    }

    pub(crate) fn set_flags(&mut self, flag: u32) {
        if let Some(extra) = self.try_as_iour_mut() {
            extra.cqe_flags = flag;
        }
    }

    #[cfg(feature = "io-uring-cqe32")]
    pub(crate) fn set_big_cqe(&mut self, big_cqe: [u64; 2]) {
        if let Some(extra) = self.try_as_iour_mut() {
            extra.big_cqe = big_cqe;
        }
    }

    pub(crate) fn set_lent_buffer(&mut self, buffer: crate::BufferRef) {
        if let Some(extra) = self.try_as_iour_mut() {
            extra.lent_buffer = Some(buffer);
        }
    }

    pub(crate) fn take_lent_buffer(&mut self) -> Option<crate::BufferRef> {
        self.try_as_iour_mut()?.lent_buffer.take()
    }
}
