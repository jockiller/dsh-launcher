//! macOS local-network privacy integration.

/// Best-effort trigger for the macOS 15+ Local Network privacy prompt.
///
/// Connecting a UDP socket is sufficient for the privacy check and does not
/// transmit a packet. Apple recommends this pattern because there is no API
/// that explicitly requests or queries the Local Network privilege.
#[cfg(target_os = "macos")]
pub fn trigger_privacy_prompt() {
    use std::net::UdpSocket;

    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return;
    };

    // This IPv4 link-local multicast address is always covered by Local
    // Network privacy. Port 9 is the discard service; connect sends nothing.
    let _ = socket.connect("224.0.0.1:9");
}
