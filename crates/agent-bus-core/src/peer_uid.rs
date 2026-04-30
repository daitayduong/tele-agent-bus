#![allow(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PeerUidError {
    #[error("peer UID {actual} does not match daemon UID {expected}")]
    Mismatch { expected: u32, actual: u32 },
    #[error("peer UID lookup failed: {0}")]
    Lookup(std::io::Error),
    #[error("peer UID lookup is unsupported on this platform")]
    Unsupported,
}

/// Mockable boundary for daemon-side Unix socket peer credential checks.
///
/// The daemon should call `verify_peer_uid` before routing any UDS request.
/// Production Linux builds use `StdPeerUid`, which calls the safe standard
/// library wrapper for `SO_PEERCRED`. Tests can inject `MockPeerUid`.
pub trait PeerUid<Conn> {
    fn peer_uid(&self, conn: &Conn) -> Result<u32, PeerUidError>;
}

pub fn verify_peer_uid<C, P>(checker: &P, conn: &C, daemon_uid: u32) -> Result<(), PeerUidError>
where
    P: PeerUid<C>,
{
    let actual = checker.peer_uid(conn)?;
    if actual == daemon_uid {
        Ok(())
    } else {
        Err(PeerUidError::Mismatch {
            expected: daemon_uid,
            actual,
        })
    }
}

#[cfg(unix)]
pub fn current_euid() -> u32 {
    // SAFETY: `geteuid` takes no pointers and has no preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub fn current_euid() -> u32 {
    0
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StdPeerUid;

#[cfg(target_os = "linux")]
impl PeerUid<std::os::unix::net::UnixStream> for StdPeerUid {
    fn peer_uid(&self, conn: &std::os::unix::net::UnixStream) -> Result<u32, PeerUidError> {
        use std::mem::MaybeUninit;
        use std::os::fd::AsRawFd;

        let mut cred = MaybeUninit::<libc::ucred>::uninit();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

        // SAFETY: `cred` points to valid writable memory sized by `len`; the
        // file descriptor comes from a live `UnixStream`; `getsockopt` writes a
        // `ucred` for `SOL_SOCKET/SO_PEERCRED` on Linux or returns -1.
        let rc = unsafe {
            libc::getsockopt(
                conn.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                cred.as_mut_ptr().cast(),
                &mut len,
            )
        };

        if rc == -1 {
            return Err(PeerUidError::Lookup(std::io::Error::last_os_error()));
        }
        if len as usize != std::mem::size_of::<libc::ucred>() {
            return Err(PeerUidError::Lookup(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected SO_PEERCRED length",
            )));
        }

        // SAFETY: `getsockopt` succeeded and reported a full `ucred` payload.
        let cred = unsafe { cred.assume_init() };
        Ok(cred.uid)
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
impl PeerUid<std::os::unix::net::UnixStream> for StdPeerUid {
    fn peer_uid(&self, _conn: &std::os::unix::net::UnixStream) -> Result<u32, PeerUidError> {
        Err(PeerUidError::Unsupported)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MockPeerUid {
    uid: u32,
}

impl MockPeerUid {
    pub fn new(uid: u32) -> Self {
        Self { uid }
    }
}

impl<C> PeerUid<C> for MockPeerUid {
    fn peer_uid(&self, _conn: &C) -> Result<u32, PeerUidError> {
        Ok(self.uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_accepts_matching_uid() {
        let checker = MockPeerUid::new(1000);

        verify_peer_uid(&checker, &(), 1000).unwrap();
    }

    #[test]
    fn mock_rejects_mismatched_uid() {
        let checker = MockPeerUid::new(1001);
        let err = verify_peer_uid(&checker, &(), 1000).unwrap_err();

        assert!(matches!(
            err,
            PeerUidError::Mismatch {
                expected: 1000,
                actual: 1001
            }
        ));
    }

    #[cfg(target_os = "linux")]
    #[cfg(unix)]
    #[test]
    fn std_peer_uid_reads_unix_stream_credential() {
        use std::os::unix::fs::MetadataExt;

        let (left, _right) = std::os::unix::net::UnixStream::pair().unwrap();
        let expected_uid = std::fs::metadata(".").unwrap().uid();

        match verify_peer_uid(&StdPeerUid, &left, expected_uid) {
            Ok(()) => {}
            Err(PeerUidError::Lookup(err))
                if err.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                eprintln!("skipping SO_PEERCRED smoke assertion: {err}");
            }
            Err(err) => panic!("unexpected peer UID error: {err}"),
        }
    }
}
