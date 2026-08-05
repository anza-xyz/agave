/// Explicit congestion notification codepoint carried in the IP header.
///
/// These values correspond to the two-bit ECN field defined by RFC 3168. They occupy the low two
/// bits of the IPv4 DS field or IPv6 Traffic Class field. To indicate non-ECT codepoint, use `None`
/// for `Option<EcnCodepoint>`.
#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum EcnCodepoint {
    /// ECN Capable Transport, codepoint 0.
    Ect0 = 0b10,
    /// ECN Capable Transport, codepoint 1.
    Ect1 = 0b01,
    /// Indicates congestion to the end nodes, set by router.
    Ce = 0b11,
}
