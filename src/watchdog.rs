//! The liveness watchdog: proof that the daemon is still turning.
//!
//! A crashed daemon is easy — systemd notices and restarts it. The failure
//! this exists for is quieter: the process stays alive, holds its listening
//! socket, and stops doing anything at all. Measured on an appliance that had
//! been in that state for eighty minutes: every thread parked on a futex, no
//! thread in `epoll_wait`, zero CPU and zero context switches over
//! twenty-five seconds, and a hundred and twenty-nine connections queued
//! against a backlog of a hundred and twenty-eight. No panic, no OOM, nothing
//! in the log. From outside it was indistinguishable from a dead machine, and
//! it needed a person with an SSH key to notice and restart it.
//!
//! systemd already knows how to restart a service that stops answering; it
//! only needs to be told the service is alive. So this pings it, and the
//! *way* it pings is the whole design: the ping is an ordinary task on the
//! async runtime, sleeping on a runtime timer. A dedicated OS thread with
//! `thread::sleep` would have gone on cheerfully reporting health while the
//! runtime it was supposed to be vouching for was dead. Only something the
//! stall would also stop can testify that there is no stall.
//!
//! Nothing here is required: with no `NOTIFY_SOCKET` in the environment —
//! run by hand, or under a supervisor that does not speak this protocol —
//! the whole thing quietly does nothing.

use std::time::Duration;

/// Tell systemd the daemon has finished starting.
///
/// Only meaningful for `Type=notify`; harmless otherwise.
pub fn notify_ready() {
    send("READY=1\n");
}

/// Feed the watchdog for as long as this future is polled.
///
/// The interval comes from `WATCHDOG_USEC`, which systemd sets from
/// `WatchdogSec=`. Pinging at half of it leaves room for one missed wake-up
/// under load without a restart, which matters on a machine that is also
/// driving four projectors.
pub async fn feed(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let Some(interval) = watchdog_interval() else {
        tracing::debug!("no watchdog configured; liveness reporting is off");
        return;
    };
    tracing::info!(?interval, "reporting liveness to systemd");

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => send("WATCHDOG=1\n"),
            _ = shutdown.changed() => return,
        }
        if *shutdown.borrow() {
            return;
        }
    }
}

/// How often to ping: half the deadline systemd is enforcing.
fn watchdog_interval() -> Option<Duration> {
    // Set only for the process systemd is actually watching. It also sets
    // WATCHDOG_PID when a service forks; honouring it keeps a child from
    // reporting on its parent's behalf.
    if let Ok(pid) = std::env::var("WATCHDOG_PID") {
        if pid.parse::<u32>() != Ok(std::process::id()) {
            return None;
        }
    }
    let micros: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    (micros > 0).then(|| Duration::from_micros(micros / 2))
}

/// Send one datagram to systemd's notification socket.
///
/// Hand-rolled rather than pulling in a crate: it is a single `sendto` to a
/// unix datagram socket, and the packaging story depends on the binary
/// staying self-contained.
fn send(message: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    // A leading '@' means the abstract namespace, which std cannot address.
    // Suede has no use for it, so say nothing rather than pretend.
    if path.to_string_lossy().starts_with('@') {
        return;
    }
    match std::os::unix::net::UnixDatagram::unbound() {
        Ok(socket) => {
            if let Err(error) = socket.send_to(message.as_bytes(), &path) {
                tracing::debug!(%error, "could not reach the systemd notify socket");
            }
        }
        Err(error) => tracing::debug!(%error, "could not open a notify socket"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment decides whether any of this runs, so the parsing is
    /// the part worth pinning down. These manipulate process-wide state, so
    /// they are one test rather than several racing ones.
    #[test]
    fn the_interval_is_half_the_deadline_and_optional() {
        // Safety: single-threaded test, and the variables are read only here.
        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
            std::env::remove_var("WATCHDOG_PID");
        }
        assert_eq!(watchdog_interval(), None, "absent means disabled");

        unsafe { std::env::set_var("WATCHDOG_USEC", "30000000") };
        assert_eq!(
            watchdog_interval(),
            Some(Duration::from_secs(15)),
            "ping at half the deadline"
        );

        unsafe { std::env::set_var("WATCHDOG_USEC", "0") };
        assert_eq!(watchdog_interval(), None, "zero means disabled");

        unsafe { std::env::set_var("WATCHDOG_USEC", "not a number") };
        assert_eq!(watchdog_interval(), None, "nonsense means disabled");

        // A value meant for a different process must not be honoured.
        unsafe {
            std::env::set_var("WATCHDOG_USEC", "30000000");
            std::env::set_var("WATCHDOG_PID", "1");
        }
        assert_eq!(watchdog_interval(), None, "not addressed to this process");

        unsafe {
            std::env::set_var("WATCHDOG_PID", std::process::id().to_string());
        }
        assert_eq!(
            watchdog_interval(),
            Some(Duration::from_secs(15)),
            "addressed to this process"
        );

        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
            std::env::remove_var("WATCHDOG_PID");
        }
    }

    /// Sending with nowhere to send must be a no-op, not a panic: the daemon
    /// runs by hand during development far more often than under systemd.
    #[test]
    fn sending_without_a_socket_is_harmless() {
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        send("WATCHDOG=1\n");

        unsafe { std::env::set_var("NOTIFY_SOCKET", "/nonexistent/suede-test.sock") };
        send("WATCHDOG=1\n");

        unsafe { std::env::set_var("NOTIFY_SOCKET", "@abstract-socket") };
        send("WATCHDOG=1\n");

        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
    }
}
