//! Traffic counters of a network interface.
//!
//! `getifaddrs`' `if_data` counters are 32-bit and wrap at 4 GiB, so this
//! asks the routing socket for the 64-bit ones (`NET_RT_IFLIST2`).

use std::ffi::CString;

/// (received, sent) bytes of the interface called `name`.
pub fn interface_bytes(name: &str) -> Option<(u64, u64)> {
    let cname = CString::new(name).ok()?;
    let index = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if index == 0 {
        return None;
    }
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, index as libc::c_int];
    let mut len: libc::size_t = 0;
    if unsafe { libc::sysctl(mib.as_mut_ptr(), mib.len() as u32, std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    if unsafe { libc::sysctl(mib.as_mut_ptr(), mib.len() as u32, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    let mut offset = 0;
    while offset + size_of::<libc::if_msghdr>() <= len {
        let hdr: libc::if_msghdr = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
        if hdr.ifm_msglen == 0 {
            break;
        }
        if i32::from(hdr.ifm_type) == libc::RTM_IFINFO2 && offset + size_of::<libc::if_msghdr2>() <= len {
            let m: libc::if_msghdr2 = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
            if u32::from(m.ifm_index) == index {
                return Some((m.ifm_data.ifi_ibytes, m.ifm_data.ifi_obytes));
            }
        }
        offset += usize::from(hdr.ifm_msglen);
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn loopback_counts_traffic() {
        let (rx0, tx0) = super::interface_bytes("lo0").expect("lo0 counters");
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = sock.local_addr().unwrap();
        for _ in 0..64 {
            sock.send_to(&[0u8; 1024], to).unwrap();
        }
        let (rx1, tx1) = super::interface_bytes("lo0").unwrap();
        assert!(rx1 >= rx0 + 64 * 1024 && tx1 >= tx0 + 64 * 1024, "{rx0}->{rx1}, {tx0}->{tx1}");
        assert!(super::interface_bytes("no-such-if0").is_none());
    }
}
