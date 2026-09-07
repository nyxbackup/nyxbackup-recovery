// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! "Be polite" socket tuning for backup traffic.
//!
//! Two cooperating mechanisms:
//!
//! 1. **DSCP / IP TOS = CS1 (8)** - marks IP packets as "scavenger / low
//!    priority". Most consumer home routers ignore this, but managed
//!    networks and a small number of QoS-aware home gateways will give
//!    foreground traffic (a video call, Netflix) priority over our
//!    backup uploads when both compete for the WAN link.
//!
//! 2. **Low-priority TCP congestion control** - yields to other TCP flows
//!    when queue delay starts rising, without needing any router help:
//!      - **Linux**: `tcp_lp` (already in mainline; loaded on demand).
//!      - **Windows 10+**: LEDBAT++, exposed via the
//!        `TCP_CONGESTION_ALGORITHM` socket option (value 6 in current
//!        SDK headers).  We tolerate failure on older Windows builds.
//!      - **macOS / other**: no-op; macOS does not expose congestion
//!        choice per-socket.
//!
//! Together they form the "don't interrupt my Netflix" defaults requested
//! in the launch checklist.  Disabled by the env var
//! `NYXBACKUP_DISABLE_NICE_NET=1` for benchmarking.

use std::io;

/// DSCP / IP TOS value used for backup traffic.
/// 8 == CS1, the "scavenger" / "lower-effort" code point (RFC 3662).
const TOS_SCAVENGER: u32 = 8;

#[cfg(target_os = "windows")]
const TCP_CONGESTION_ALGORITHM_LEDBAT: u32 = 6; // CongestionAlgorithm enum, Win10+

/// Returns true unless the user opted out via env var.
pub fn nice_net_enabled() -> bool {
    match std::env::var("NYXBACKUP_DISABLE_NICE_NET") {
        Ok(v) => v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// Apply DSCP + low-priority congestion to a connected `std::net::TcpStream`.
/// Best-effort: errors are returned to the caller, which may log and continue.
pub fn apply_to_tcp_stream(stream: &std::net::TcpStream) -> io::Result<()> {
    if !nice_net_enabled() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        unsafe {
            set_options(stream.as_raw_fd())?;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        unsafe {
            set_options(stream.as_raw_socket() as usize)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
unsafe fn set_options(fd: i32) -> io::Result<()> {
    unsafe {
        // IP_TOS - DSCP CS1 (scavenger).
        let tos = TOS_SCAVENGER as libc::c_int;
        let r = libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_TOS,
            &tos as *const _ as *const libc::c_void,
            std::mem::size_of_val(&tos) as libc::socklen_t,
        );
        if r != 0 {
            // Continue even if IP_TOS fails (e.g. CAP_NET_ADMIN required on
            // some kernels for higher values; CS1 is normally permitted).
            let _ = io::Error::last_os_error();
        }

        // TCP_CONGESTION = "lp" (low-priority).
        #[cfg(target_os = "linux")]
        {
            let name = b"lp\0";
            let r = libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_CONGESTION,
                name.as_ptr() as *const libc::c_void,
                (name.len() - 1) as libc::socklen_t,
            );
            if r != 0 {
                // Non-fatal: tcp_lp module not loaded, etc.
                let _ = io::Error::last_os_error();
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
unsafe fn set_options(sock: usize) -> io::Result<()> {
    // Windows uses winsock2 setsockopt; the constants live in the
    // ws2_32 library.  We use bindgen-less raw FFI to avoid a heavy
    // dependency for two setsockopt calls.
    type Socket = usize;
    const SOL_IP: i32 = 0;
    const IP_TOS_W: i32 = 3; // identical to POSIX value
    const IPPROTO_TCP_W: i32 = 6;
    // The TCP_CONGESTION_ALGORITHM option number is documented in
    // mstcpip.h as 0x18 (24).  We avoid binding to the header and
    // tolerate failure on Windows builds that don't recognise it.
    const TCP_CONGESTION_ALGORITHM_OPT: i32 = 24;

    unsafe extern "system" {
        fn setsockopt(s: Socket, level: i32, optname: i32, optval: *const u8, optlen: i32) -> i32;
    }

    // IP_TOS
    let tos: u32 = TOS_SCAVENGER;
    unsafe {
        let _ = setsockopt(
            sock,
            SOL_IP,
            IP_TOS_W,
            &tos as *const _ as *const u8,
            std::mem::size_of_val(&tos) as i32,
        );
    }

    // TCP_CONGESTION_ALGORITHM = LEDBAT (best-effort on Win10+; ignored
    // by older builds and returns WSAENOPROTOOPT).
    let alg: u32 = TCP_CONGESTION_ALGORITHM_LEDBAT;
    unsafe {
        let _ = setsockopt(
            sock,
            IPPROTO_TCP_W,
            TCP_CONGESTION_ALGORITHM_OPT,
            &alg as *const _ as *const u8,
            std::mem::size_of_val(&alg) as i32,
        );
    }
    Ok(())
}
