//! Experimental Vulkan image transport. One Unix seqpacket per fixed-size
//! message, with three owned OPAQUE_FD handles attached to each pool slot.
//! Pixels never cross this socket. Disconnects and malformed packets end the
//! renderer rather than leaving an apparently live cached picture on screen.
use anyhow::{bail, ensure, Context};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub use crate::model::{SharedInputMode, SharedInputOptions};

pub const SLOT_COUNT: usize = 3;
const PACKET_BYTES: usize = 128;
const MAGIC: u32 = 0x5355_4544;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotDescription {
    pub slot: usize,
    pub width: u32,
    pub height: u32,
    pub format: i32,
    pub memory_type: u32,
    pub allocation_size: u64,
    pub device_uuid: [u8; 16],
}

#[derive(Debug)]
pub enum Packet {
    Slot(SlotDescription, [OwnedFd; 3]),
    Frame { slot: usize, sequence: u64 },
}

fn decode(bytes: &[u8; PACKET_BYTES], fds: Vec<OwnedFd>) -> anyhow::Result<Packet> {
    let words: Vec<_> = bytes
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect();
    ensure!(
        words[0] == MAGIC && words[1] == 1,
        "shared texture: invalid magic/version"
    );
    let slot = words[3] as usize;
    ensure!(slot < SLOT_COUNT, "shared texture: invalid slot {slot}");
    match words[2] {
        1 => {
            ensure!(
                fds.len() == 3,
                "shared texture: slot requires exactly three FDs"
            );
            ensure!(
                words[14..].iter().all(|&w| w == 0),
                "shared texture: nonzero reserved slot fields"
            );
            ensure!(
                words[4] > 0 && words[5] > 0 && words[4] <= 16384 && words[5] <= 16384,
                "shared texture: invalid image bounds"
            );
            let allocation_size = u64::from(words[8]) | (u64::from(words[9]) << 32);
            ensure!(
                allocation_size > 0 && allocation_size <= 8 * 1024 * 1024 * 1024,
                "shared texture: allocation size out of bounds"
            );
            Ok(Packet::Slot(
                SlotDescription {
                    slot,
                    width: words[4],
                    height: words[5],
                    format: words[6] as i32,
                    memory_type: words[7],
                    allocation_size,
                    device_uuid: bytes[40..56].try_into().unwrap(),
                },
                fds.try_into().unwrap(),
            ))
        }
        2 => {
            ensure!(fds.is_empty(), "shared texture: frame must not carry FDs");
            ensure!(
                words[6..].iter().all(|&w| w == 0),
                "shared texture: nonzero reserved frame fields"
            );
            Ok(Packet::Frame {
                slot,
                sequence: u64::from(words[4]) | (u64::from(words[5]) << 32),
            })
        }
        other => bail!("shared texture: unexpected packet type {other}"),
    }
}

pub struct Receiver {
    socket: OwnedFd,
    registered: [bool; SLOT_COUNT],
    sequence: Option<u64>,
}

impl Receiver {
    pub fn connect(path: &Path) -> anyhow::Result<Self> {
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let bytes = path.as_os_str().as_bytes();
        ensure!(
            !bytes.is_empty() && bytes.len() < address.sun_path.len() && !bytes.contains(&0),
            "shared texture: invalid Unix socket path"
        );
        address.sun_family = libc::AF_UNIX as _;
        for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
            *target = *byte as _;
        }
        let fd =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        // A full accept backlog must join the bounded startup retry rather
        // than blocking connect and preventing shutdown/test deadlines.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("making shared producer connection nonblocking");
        }
        let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
        if unsafe { libc::connect(fd, &address as *const _ as *const _, length as _) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("connecting shared texture producer");
        }
        Ok(Self {
            socket,
            registered: [false; SLOT_COUNT],
            sequence: None,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }

    /// Observe disconnects while a GPU copy is pending without consuming
    /// the next frame or waking repeatedly on already queued frame packets.
    pub fn check_connection(&self) -> anyhow::Result<()> {
        let mut descriptor = libc::pollfd {
            fd: self.fd(),
            events: 0,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
        ensure!(
            descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0,
            "shared texture: producer disconnected"
        );
        Ok(())
    }

    pub fn receive(&mut self) -> anyhow::Result<Option<Packet>> {
        let mut bytes = [0u8; PACKET_BYTES];
        // Aligned storage, bounded to a handful of descriptors. MSG_CTRUNC
        // closes excess descriptors in the kernel; received ones enter RAII
        // before any validation so errors cannot leak them.
        let mut control = [0usize; 16];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control);
        let received = unsafe {
            libc::recvmsg(
                self.fd(),
                &mut message,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        if received < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                return Ok(None);
            }
            return Err(error.into());
        }
        let mut fds = Vec::new();
        let mut unexpected_control = false;
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&message);
            while !header.is_null() {
                let item = &*header;
                if item.cmsg_level == libc::SOL_SOCKET && item.cmsg_type == libc::SCM_RIGHTS {
                    let length = item.cmsg_len.saturating_sub(libc::CMSG_LEN(0) as usize);
                    for index in 0..length / std::mem::size_of::<RawFd>() {
                        let fd = std::ptr::read_unaligned(
                            libc::CMSG_DATA(header).cast::<RawFd>().add(index),
                        );
                        fds.push(OwnedFd::from_raw_fd(fd));
                    }
                } else {
                    unexpected_control = true;
                }
                header = libc::CMSG_NXTHDR(&message, header);
            }
        }
        ensure!(received > 0, "shared texture: producer disconnected");
        ensure!(
            received as usize == PACKET_BYTES
                && message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0,
            "shared texture: truncated or incorrectly sized packet"
        );
        ensure!(
            !unexpected_control,
            "shared texture: unexpected ancillary data"
        );
        let packet = decode(&bytes, fds)?;
        match &packet {
            Packet::Slot(description, _) => {
                ensure!(
                    !self.registered[description.slot] && self.sequence.is_none(),
                    "shared texture: duplicate/late slot"
                );
                self.registered[description.slot] = true;
            }
            Packet::Frame { sequence, .. } => {
                ensure!(
                    self.registered.iter().all(|&slot| slot),
                    "shared texture: frame before all three slots"
                );
                ensure!(
                    self.sequence.is_none_or(|previous| *sequence > previous),
                    "shared texture: non-increasing sequence"
                );
                self.sequence = Some(*sequence);
            }
        }
        Ok(Some(packet))
    }

    /// Called only after the consumer's acquire/copy/release/done submission
    /// has been accepted. The producer still waits the GPU done semaphore.
    pub fn release(&self, slot: usize, sequence: u64) -> anyhow::Result<()> {
        ensure!(slot < SLOT_COUNT, "shared texture: invalid release slot");
        self.send_control([
            MAGIC,
            1,
            3,
            slot as u32,
            sequence as u32,
            (sequence >> 32) as u32,
        ])
    }

    /// One explicit consumer-paced scene credit, issued after importing the
    /// pool or completing presentation. Default unpaced inputs send none.
    pub fn grant_credit(&self) -> anyhow::Result<()> {
        self.send_control([MAGIC, 1, 4, 0, 1, 0])
    }

    fn send_control(&self, words: [u32; 6]) -> anyhow::Result<()> {
        let mut bytes = [0u8; PACKET_BYTES];
        for (index, word) in words.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        let sent =
            unsafe { libc::sendmsg(self.fd(), &message, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
        ensure!(
            sent == PACKET_BYTES as isize,
            "shared texture: control send failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> (Receiver, OwnedFd) {
        let mut sockets = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );
        (
            Receiver {
                socket: unsafe { OwnedFd::from_raw_fd(sockets[0]) },
                registered: [false; SLOT_COUNT],
                sequence: None,
            },
            unsafe { OwnedFd::from_raw_fd(sockets[1]) },
        )
    }

    fn send(socket: &OwnedFd, bytes: &[u8], fds: &[RawFd]) {
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut _,
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        let length = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as _) } as usize;
        let mut control = vec![0usize; length.div_ceil(std::mem::size_of::<usize>())];
        if !fds.is_empty() {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = length;
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as _) as _;
                std::ptr::copy_nonoverlapping(
                    fds.as_ptr(),
                    libc::CMSG_DATA(header).cast(),
                    fds.len(),
                );
            }
        }
        assert_eq!(
            unsafe { libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL) },
            bytes.len() as isize
        );
    }

    fn slot(index: u32) -> [u8; PACKET_BYTES] {
        let mut bytes = frame(index, 0);
        for (index, value) in [
            (2, 1u32),
            (4, 1920),
            (5, 1200),
            (6, 44),
            (7, 1),
            (8, 16_000_000),
        ] {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn frame(slot: u32, sequence: u64) -> [u8; PACKET_BYTES] {
        let mut result = [0; PACKET_BYTES];
        for (index, value) in [MAGIC, 1, 2, slot, sequence as u32, (sequence >> 32) as u32]
            .iter()
            .enumerate()
        {
            result[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        result
    }
    #[test]
    fn validates_frame_metadata() {
        assert!(
            matches!(decode(&frame(2, 1 << 40), vec![]).unwrap(), Packet::Frame { slot: 2, sequence } if sequence == 1 << 40)
        );
        assert!(decode(&frame(3, 1), vec![]).is_err());
        let mut reserved = frame(0, 1);
        reserved[127] = 1;
        assert!(decode(&reserved, vec![]).is_err());
        let fd = std::fs::File::open("/dev/null").unwrap().into();
        assert!(decode(&frame(0, 1), vec![fd]).is_err());
    }

    #[test]
    fn pool_frame_and_release_round_trip() {
        let (mut receiver, producer) = pair();
        receiver.check_connection().unwrap();
        assert!(receiver.receive().unwrap().is_none());
        let file = std::fs::File::open("/dev/null").unwrap();
        for index in 0..SLOT_COUNT {
            send(&producer, &slot(index as u32), &[file.as_raw_fd(); 3]);
            match receiver.receive().unwrap().unwrap() {
                Packet::Slot(description, fds) => {
                    assert_eq!(description.slot, index);
                    assert_eq!((description.width, description.height), (1920, 1200));
                    assert!(fds
                        .iter()
                        .all(|fd| unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) }
                            & libc::FD_CLOEXEC
                            != 0));
                }
                _ => panic!("expected slot"),
            }
        }
        send(&producer, &frame(2, 7), &[]);
        // Queued input is deliberately ignored by the pending-copy
        // connection check, preventing a producer packet from hot-polling.
        receiver.check_connection().unwrap();
        assert!(matches!(
            receiver.receive().unwrap(),
            Some(Packet::Frame {
                slot: 2,
                sequence: 7
            })
        ));
        receiver.release(2, 7).unwrap();
        let mut reply = [0u8; PACKET_BYTES];
        assert_eq!(
            unsafe {
                libc::recv(
                    producer.as_raw_fd(),
                    reply.as_mut_ptr().cast(),
                    reply.len(),
                    0,
                )
            },
            PACKET_BYTES as isize
        );
        let mut expected = frame(2, 7);
        expected[8..12].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(reply, expected);
        receiver.grant_credit().unwrap();
        assert_eq!(
            unsafe {
                libc::recv(
                    producer.as_raw_fd(),
                    reply.as_mut_ptr().cast(),
                    reply.len(),
                    0,
                )
            },
            PACKET_BYTES as isize
        );
        let mut expected = frame(0, 1);
        expected[8..12].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(
            reply, expected,
            "credit has no slot, sequence, or reserved payload"
        );
        send(&producer, &frame(0, 7), &[]);
        assert!(receiver
            .receive()
            .unwrap_err()
            .to_string()
            .contains("sequence"));
    }

    #[test]
    fn rejects_early_frames_duplicate_slots_and_disconnect() {
        let (mut receiver, producer) = pair();
        send(&producer, &frame(0, 1), &[]);
        assert!(receiver
            .receive()
            .unwrap_err()
            .to_string()
            .contains("before all three"));
        let (mut receiver, producer) = pair();
        let file = std::fs::File::open("/dev/null").unwrap();
        send(&producer, &slot(0), &[file.as_raw_fd(); 3]);
        assert!(receiver.receive().unwrap().is_some());
        send(&producer, &slot(0), &[file.as_raw_fd(); 3]);
        assert!(receiver
            .receive()
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
        drop(producer);
        assert!(receiver.check_connection().is_err());
        assert!(receiver
            .receive()
            .unwrap_err()
            .to_string()
            .contains("disconnected"));
    }

    #[test]
    fn rejects_truncated_packets_and_closes_owned_fds_on_parse_error() {
        let (mut receiver, producer) = pair();
        send(&producer, &[0u8; PACKET_BYTES + 1], &[]);
        assert!(receiver
            .receive()
            .unwrap_err()
            .to_string()
            .contains("sized"));
        let file = std::fs::File::open("/dev/null").unwrap();
        send(&producer, &slot(0), &[file.as_raw_fd(); 40]);
        assert!(receiver
            .receive()
            .unwrap_err()
            .to_string()
            .contains("truncated"));
        // EOF proves that parsing closed the last pipe writer. Descriptor
        // number reuse by concurrent tests cannot change this observation.
        let mut pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let reader = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
        assert_eq!(
            unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        assert!(decode(&frame(0, 1), vec![writer]).is_err());
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::read(reader.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            0
        );
    }

    #[test]
    fn rejects_invalid_slot_metadata_and_wrong_fd_counts() {
        let fds =
            || std::array::from_fn::<_, 3, _>(|_| std::fs::File::open("/dev/null").unwrap().into());
        let mut bytes = slot(0);
        bytes[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert!(decode(&bytes, Vec::from(fds())).is_err());
        let mut bytes = slot(0);
        bytes[56] = 1;
        assert!(decode(&bytes, Vec::from(fds())).is_err());
        assert!(decode(&slot(0), Vec::new()).is_err());
    }
}
