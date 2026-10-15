//! Traffic counters of a network adapter, and console control for openconnect.

use anyhow::Result;
use windows::Win32::NetworkManagement::IpHelper::{ConvertInterfaceAliasToLuid, GetIfEntry2, MIB_IF_ROW2};
use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows::Win32::System::Console::{AttachConsole, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler};
use windows::core::HSTRING;

/// (received, sent) bytes of the adapter called `alias`.
pub fn interface_bytes(alias: &str) -> Option<(u64, u64)> {
    let alias = HSTRING::from(alias);
    let mut luid = NET_LUID_LH::default();
    unsafe { ConvertInterfaceAliasToLuid(&alias, &mut luid) }.ok().ok()?;
    let mut row = MIB_IF_ROW2 { InterfaceLuid: luid, ..Default::default() };
    unsafe { GetIfEntry2(&mut row) }.ok().ok()?;
    Some((row.InOctets, row.OutOctets))
}

/// `oc-stop <pid>`: delivers Ctrl+C to a console process (openconnect.exe),
/// which makes it disconnect cleanly.
pub fn send_ctrl_c(pid: u32) -> Result<()> {
    unsafe {
        let _ = FreeConsole();
        AttachConsole(pid)?;
        // Don't let the Ctrl+C stop this helper too.
        SetConsoleCtrlHandler(None, true)?;
        GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)?;
        let _ = FreeConsole();
    }
    Ok(())
}
