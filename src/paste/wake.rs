// Waiting in the paste threads: for the display connection and for a
// command from the app at the same time, without a timer ticking.

use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

/// Wakes a paste thread when a command is waiting.
pub struct Wake {
    pub fd: RawFd,
}

impl Wake {
    pub fn new() -> io::Result<Wake> {
        // SAFETY: eventfd takes plain flags and returns a new descriptor.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Wake { fd })
    }

    pub fn ring(&self) {
        let one: u64 = 1;
        // SAFETY: eight bytes from a live u64 to our own eventfd.
        unsafe { libc::write(self.fd, (&raw const one).cast(), 8) };
    }

    pub fn clear(&self) {
        let mut value: u64 = 0;
        // SAFETY: eight bytes into a live u64 from our own eventfd.
        unsafe { libc::read(self.fd, (&raw mut value).cast(), 8) };
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: the descriptor is ours and closed once.
        unsafe { libc::close(self.fd) };
    }
}

/// Sleeps until one of the descriptors can be read, or the time is up; says
/// which ones can.
pub fn wait_readable(fds: &[RawFd], timeout: Option<Duration>) -> Vec<bool> {
    let mut polls: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
        .collect();
    let milliseconds = timeout.map_or(-1, |timeout| {
        i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX)
    });
    // SAFETY: the array lives through the call and its length is passed.
    unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, milliseconds) };
    polls
        .iter()
        .map(|poll| poll.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
        .collect()
}
