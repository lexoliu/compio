//! A buffer pool registered with `IOU_PBUF_RING_INC` lends each completion
//! the part of a buffer it filled, and gives the buffer back to the ring only
//! once the kernel is done with it and nothing refers to it.

#![cfg(io_uring)]

use std::{
    io::{self, Write},
    net::Shutdown,
    num::NonZeroU16,
    os::unix::net::UnixStream,
};

use compio_buf::BufResult;
use compio_driver::{
    AsRawFd, BufferPool, BufferRef, OpCode, Proactor, ProactorBuilder, PushEntry, ResultTakeBuffer,
    SharedFd,
    op::{RecvManaged, RecvMulti},
};
use io_uring::types::IOU_PBUF_RING_INC;
use rustix::net::RecvFlags;

const BUFFER_LEN: usize = 4096;

fn push_and_wait<O: OpCode + 'static>(driver: &mut Proactor, op: O) -> BufResult<usize, O> {
    match driver.push(op) {
        PushEntry::Ready(res) => res,
        PushEntry::Pending(mut key) => loop {
            driver.poll(None).unwrap();
            match driver.pop(key) {
                PushEntry::Pending(k) => key = k,
                PushEntry::Ready(res) => break res,
            }
        },
    }
}

struct Setup {
    driver: Proactor,
    pool: BufferPool,
    rx: SharedFd<UnixStream>,
    tx: UnixStream,
}

/// A driver whose pool is a single incrementally consumed buffer, and a
/// connected socket pair attached to it.
fn setup() -> Option<Setup> {
    let mut driver = ProactorBuilder::new()
        .buffer_pool_size(NonZeroU16::new(1).unwrap())
        .buffer_pool_buffer_len(BUFFER_LEN)
        .buffer_pool_flag(IOU_PBUF_RING_INC as _)
        .build()
        .unwrap();
    if !driver.driver_type().is_iouring() {
        eprintln!("the driver is not io_uring, skipping test");
        return None;
    }
    let pool = driver.buffer_pool().unwrap();
    if !pool.is_incremental().unwrap() {
        eprintln!("the kernel rejected IOU_PBUF_RING_INC (before Linux 6.12), skipping test");
        return None;
    }
    let (rx, tx) = UnixStream::pair().unwrap();
    let rx = SharedFd::new(rx);
    driver.attach(rx.as_raw_fd()).unwrap();
    Some(Setup {
        driver,
        pool,
        rx,
        tx,
    })
}

impl Setup {
    fn recv(&mut self) -> io::Result<BufferRef> {
        let op = RecvManaged::new(self.rx.clone(), &self.pool, 0, RecvFlags::empty()).unwrap();
        let buffer = unsafe { push_and_wait(&mut self.driver, op).take_buffer() }?;
        Ok(buffer.expect("the peer is still connected"))
    }

    fn send(&mut self, data: &[u8]) -> io::Result<BufferRef> {
        self.tx.write_all(data).unwrap();
        self.recv()
    }
}

#[test]
fn consecutive_receives_share_a_buffer() {
    let Some(mut s) = setup() else { return };
    let first = s.send(b"hello").unwrap();
    let second = s.send(b"world").unwrap();
    assert_eq!(&*first, b"hello");
    assert_eq!(&*second, b"world");
    assert_eq!(second.as_ptr(), first.as_ptr().wrapping_add(first.len()));
}

#[test]
fn buffer_returns_once_the_kernel_is_done() {
    let Some(mut s) = setup() else { return };
    let mut parts = vec![s.send(&[1; 10]).unwrap()];
    let start = parts[0].as_ptr();
    s.tx.write_all(&[2; BUFFER_LEN - 10]).unwrap();
    while parts.iter().map(|part| part.len()).sum::<usize>() < BUFFER_LEN {
        parts.push(s.recv().unwrap());
    }

    // The kernel has filled the buffer and given it up, but the parts still
    // refer to it, so the ring is empty.
    s.tx.write_all(b"next").unwrap();
    let err = s.recv().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ResourceBusy, "{err}");

    drop(parts);
    let next = s.recv().unwrap();
    assert_eq!(&*next, b"next");
    assert_eq!(next.as_ptr(), start);
}

/// Two multishot receives fill one buffer in the order the kernel posts their
/// completions, and each completion keeps its own bytes whichever of them is
/// taken first.
#[test]
fn multishot_completions_keep_kernel_order() {
    let Some(mut s) = setup() else { return };
    let (other_rx, mut other_tx) = UnixStream::pair().unwrap();
    let other_rx = SharedFd::new(other_rx);
    s.driver.attach(other_rx.as_raw_fd()).unwrap();

    let first = RecvMulti::new(s.rx.clone(), &s.pool, 0, RecvFlags::empty()).unwrap();
    let PushEntry::Pending(first) = s.driver.push(first) else {
        panic!("a multishot receive completes asynchronously");
    };
    let second = RecvMulti::new(other_rx.clone(), &s.pool, 0, RecvFlags::empty()).unwrap();
    let PushEntry::Pending(second) = s.driver.push(second) else {
        panic!("a multishot receive completes asynchronously");
    };

    s.tx.write_all(b"hello").unwrap();
    let mut hello = loop {
        s.driver.poll(None).unwrap();
        if let Some(res) = s.driver.pop_multishot(&first) {
            break res;
        }
    };
    other_tx.write_all(b"world").unwrap();
    let mut world = loop {
        s.driver.poll(None).unwrap();
        if let Some(res) = s.driver.pop_multishot(&second) {
            break res;
        }
    };

    // Take the later completion first.
    let world_len = *world.0.as_ref().unwrap();
    let world = s.pool.take(&mut world.1, world_len).unwrap().unwrap();
    let hello_len = *hello.0.as_ref().unwrap();
    let hello = s.pool.take(&mut hello.1, hello_len).unwrap().unwrap();
    assert_eq!(&*hello, b"hello");
    assert_eq!(&*world, b"world");
    assert_eq!(world.as_ptr(), hello.as_ptr().wrapping_add(hello.len()));

    drop((hello, world));
    s.tx.shutdown(Shutdown::Write).unwrap();
    other_tx.shutdown(Shutdown::Write).unwrap();
    for mut key in [first, second] {
        while let PushEntry::Pending(k) = s.driver.pop(key) {
            key = k;
            s.driver.poll(None).unwrap();
        }
    }
}

#[test]
fn reference_outlives_driver() {
    let Some(mut s) = setup() else { return };
    let first = s.send(b"hello").unwrap();
    let second = s.send(b"world").unwrap();
    drop(s);
    assert_eq!(&*first, b"hello");
    assert_eq!(&*second, b"world");
}
