use crate::{err, Result};
use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::Path,
};

pub(crate) struct CodexPeer {
    credentials: libc::ucred,
    pidfd: OwnedFd,
}

impl CodexPeer {
    pub(crate) fn connect(socket: &Path, uid: u32) -> Result<Self> {
        let stream = UnixStream::connect(socket)?;
        let credentials = credentials(&stream)?;
        if credentials.pid <= 1 || credentials.uid != uid {
            return Err(err(
                "control socket peer is not a visible process owned by this user",
            ));
        }

        // Obtain the process handle from the socket itself, without reopening a numeric PID.
        let mut fd: libc::c_int = -1;
        let mut len = size_of_val(&fd) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut fd as *mut libc::c_int).cast(),
                &mut len,
            )
        } == -1
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOPROTOOPT) {
                return Err(err(
                    "cannot pin the socket peer: requires Linux 6.5+ (SO_PEERPIDFD)",
                ));
            }
            return Err(err(format!("cannot pin the socket peer: {error}")));
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd) };

        let proc = std::path::PathBuf::from(format!("/proc/{}", credentials.pid));
        let exe = fs::read_link(proc.join("exe"))?;
        let cmdline = fs::read(proc.join("cmdline"))?;
        let mut args = cmdline.split(|byte| *byte == 0).skip(1);
        if !exe
            .file_name()
            .is_some_and(|name| matches!(name.as_encoded_bytes(), b"codex" | b"codex (deleted)"))
            || args.next() != Some(b"app-server".as_slice())
            || args.any(|arg| matches!(arg, b"proxy" | b"daemon"))
        {
            return Err(err(format!(
                "control socket peer PID {} is not a Codex app-server process",
                credentials.pid
            )));
        }
        Ok(Self { credentials, pidfd })
    }

    pub(crate) fn pid(&self) -> libc::pid_t {
        self.credentials.pid
    }

    pub(crate) fn stop(&self, socket: &Path, timeout: u64) -> Result<()> {
        if self.exited(0)? {
            return Ok(());
        }
        let current = credentials(&UnixStream::connect(socket)?)?;
        if (current.pid, current.uid) != (self.credentials.pid, self.credentials.uid) {
            return Err(err("Codex control socket listener changed during recovery"));
        }

        eprintln!(
            "Stopping unmanaged Codex app-server PID {} (SIGTERM).",
            self.credentials.pid
        );
        self.signal(libc::SIGTERM)?;
        let wait_ms = (timeout.min(5) * 1000) as libc::c_int;
        if !self.exited(wait_ms)? {
            eprintln!(
                "Codex app-server PID {} did not exit (SIGKILL).",
                self.credentials.pid
            );
            self.signal(libc::SIGKILL)?;
            if !self.exited(wait_ms)? {
                return Err(err("Codex app-server did not exit after SIGKILL"));
            }
        }
        Ok(())
    }

    fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0u32,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    fn exited(&self, wait_ms: libc::c_int) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, wait_ms) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(poll.revents & libc::POLLIN != 0)
    }
}

fn credentials(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut peer = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = size_of_val(&peer) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut len,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(peer)
}
