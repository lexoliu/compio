use std::io::{self, IoSliceMut};

use compio_driver::Proactor;

/// A leaked, zeroed region of `len` bytes, which outlives its registration
/// and every operation on it.
fn region(len: usize) -> &'static mut [u8] {
    Box::leak(vec![0; len].into_boxed_slice())
}

#[cfg(not(io_uring))]
#[test]
fn register_buffers_unsupported() {
    let driver = Proactor::new().unwrap();
    let region = region(4096);
    // SAFETY: the region is leaked.
    let res = unsafe { driver.register_buffers(&[IoSliceMut::new(region)]) };
    assert_eq!(res.unwrap_err().kind(), io::ErrorKind::Unsupported);
    assert_eq!(
        driver.unregister_buffers().unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
}

#[cfg(io_uring)]
mod iour {
    use std::{
        io::Read,
        net::{TcpListener, TcpStream},
        os::fd::AsRawFd,
    };

    use compio_buf::{BufResult, IntoInner};
    use compio_driver::{
        OpCode, PushEntry, SharedFd,
        op::{ReadFixedAt, SendVectoredZcFixed, SendZcFixed},
    };
    use rustix::net::SendFlags;

    use super::*;

    /// A proactor on io-uring, or `None` when the kernel refused it and the
    /// fused driver fell back to polling, where registering is unsupported.
    fn iouring() -> Option<Proactor> {
        let driver = Proactor::new().unwrap();
        if driver.driver_type().is_iouring() {
            return Some(driver);
        }
        let region = region(4096);
        // SAFETY: the region is leaked.
        let res = unsafe { driver.register_buffers(&[IoSliceMut::new(region)]) };
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::Unsupported);
        eprintln!("Current driver does not support registered buffers, skipping test");
        None
    }

    /// Registers `region` as buffer 0 and returns the address it starts at.
    fn register(driver: &Proactor, region: &'static mut [u8]) -> *mut u8 {
        let ptr = region.as_mut_ptr();
        // SAFETY: the region is leaked.
        unsafe { driver.register_buffers(&[IoSliceMut::new(region)]) }.unwrap();
        ptr
    }

    /// `len` bytes of a registered region from `offset`.
    ///
    /// # Safety
    ///
    /// The bytes lie inside a leaked region.
    unsafe fn part(ptr: *mut u8, offset: usize, len: usize) -> &'static [u8] {
        unsafe { std::slice::from_raw_parts(ptr.add(offset), len) }
    }

    fn wait<O: OpCode + 'static>(driver: &mut Proactor, op: O) -> BufResult<usize, O> {
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

    /// Waits for a zerocopy send and its notification, and returns what the
    /// send itself completed with.
    fn wait_zc<O: OpCode + 'static>(driver: &mut Proactor, op: O) -> io::Result<usize> {
        let mut key = match driver.push(op) {
            PushEntry::Ready(BufResult(res, _)) => return res,
            PushEntry::Pending(key) => key,
        };
        let mut sent = None;
        loop {
            driver.poll(None).unwrap();
            if let Some(BufResult(res, _)) = driver.pop_multishot(&key) {
                sent = Some(res);
            }
            match driver.pop(key) {
                PushEntry::Pending(k) => key = k,
                PushEntry::Ready(BufResult(res, _)) => break sent.unwrap_or(res),
            }
        }
    }

    /// A connected socket the driver sends on, and the peer's end.
    fn connected(driver: &mut Proactor) -> (SharedFd<socket2::Socket>, TcpStream) {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        let (peer, _) = server.accept().unwrap();
        let stream = SharedFd::new(socket2::Socket::from(stream));
        driver.attach(stream.as_raw_fd()).unwrap();
        (stream, peer)
    }

    #[test]
    fn register_unregister() {
        let Some(driver) = iouring() else { return };
        register(&driver, region(4096));
        // A ring holds one table.
        // SAFETY: the region is leaked.
        let again = unsafe { driver.register_buffers(&[IoSliceMut::new(region(4096))]) };
        assert_eq!(again.unwrap_err().raw_os_error(), Some(libc::EBUSY));
        driver.unregister_buffers().unwrap();
        register(&driver, region(4096));
        driver.unregister_buffers().unwrap();
    }

    #[test]
    fn read_fixed() {
        let Some(mut driver) = iouring() else { return };
        let expected = std::fs::read("Cargo.toml").unwrap();
        let ptr = register(&driver, region(expected.len() + 4096));

        let file = SharedFd::new(std::fs::File::open("Cargo.toml").unwrap());
        driver.attach(file.as_raw_fd()).unwrap();
        // SAFETY: the bytes lie inside the leaked region, which nothing else
        // uses meanwhile.
        let target = unsafe { std::slice::from_raw_parts_mut(ptr.add(4096), expected.len()) };
        let BufResult(res, op) = wait(&mut driver, ReadFixedAt::new(file, 0, target, 0));
        assert_eq!(res.unwrap(), expected.len());
        assert_eq!(&*op.into_inner(), &expected[..]);
    }

    #[test]
    fn read_fixed_outside_the_buffer() {
        let Some(mut driver) = iouring() else { return };
        register(&driver, region(4096));
        let file = SharedFd::new(std::fs::File::open("Cargo.toml").unwrap());
        driver.attach(file.as_raw_fd()).unwrap();
        // Memory the table does not hold: the kernel refuses it.
        let BufResult(res, _) = wait(&mut driver, ReadFixedAt::new(file, 0, region(64), 0));
        assert_eq!(res.unwrap_err().raw_os_error(), Some(libc::EFAULT));
    }

    #[test]
    fn send_zc_fixed() {
        let Some(mut driver) = iouring() else { return };
        let ptr = register(&driver, region(4096));
        // SAFETY: the bytes lie inside the leaked region.
        unsafe { std::slice::from_raw_parts_mut(ptr.add(100), 12) }
            .copy_from_slice(b"Hello world!");
        let (stream, mut peer) = connected(&mut driver);

        // SAFETY: the bytes lie inside the leaked region.
        let buf = unsafe { part(ptr, 100, 12) };
        let sent = wait_zc(
            &mut driver,
            SendZcFixed::new(stream, buf, 0, SendFlags::empty()),
        );
        assert_eq!(sent.unwrap(), 12);
        let mut received = [0; 12];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"Hello world!");
    }

    #[test]
    fn send_vectored_zc_fixed() {
        let Some(mut driver) = iouring() else { return };
        let ptr = register(&driver, region(8192));
        // SAFETY: the bytes lie inside the leaked region.
        unsafe {
            std::slice::from_raw_parts_mut(ptr, 6).copy_from_slice(b"Hello ");
            std::slice::from_raw_parts_mut(ptr.add(5000), 6).copy_from_slice(b"world!");
        }
        let (stream, mut peer) = connected(&mut driver);

        // SAFETY: the bytes lie inside the leaked region.
        let bufs = unsafe { [part(ptr, 0, 6), part(ptr, 5000, 6)] };
        let op = SendVectoredZcFixed::new(stream, bufs, 0, SendFlags::empty());
        match wait_zc(&mut driver, op) {
            // Before 6.15 the kernel takes no registered buffer on a vectored
            // zerocopy send, and says so before sending anything.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                eprintln!("Kernel does not support vectored registered buffers, skipping test");
                return;
            }
            sent => assert_eq!(sent.unwrap(), 12),
        }
        let mut received = [0; 12];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"Hello world!");
    }
}
