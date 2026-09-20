use std::num::NonZeroU16;

/// A relation-local address of one physical tuple version.
///
/// Pages are zero-based stored-page identifiers, not buffer-frame indices.
/// Slots are one-based tuple-directory entries, not byte offsets. The relation
/// comes from the enclosing scan or ModifyTable target.
/// All u32 page IDs are representable; storage must validate that the address
/// exists and still identifies the expected version.
///
/// A CTID is not a permanent row ID, visibility proof, lock, or buffer pin.
/// UPDATE can produce a different CTID, and reclamation can reuse an address.
/// Executors must validate visibility and concurrent modification before modifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ctid {
    pub page_id: u32,
    pub slot_id: NonZeroU16,
}

impl Ctid {
    /// Width of the canonical execution-interchange encoding, not Rust's layout
    /// or a commitment to PostgreSQL's ABI or an on-disk page format.
    pub const BYTE_LEN: usize = 6;

    pub const fn new(page_id: u32, slot_id: u16) -> Option<Self> {
        match NonZeroU16::new(slot_id) {
            Some(slot_id) => Some(Self { page_id, slot_id }),
            None => None,
        }
    }

    /// Page ID followed by slot ID, both little-endian; never serialize Rust padding.
    pub const fn to_le_bytes(self) -> [u8; Self::BYTE_LEN] {
        let page = self.page_id.to_le_bytes();
        let slot = self.slot_id.get().to_le_bytes();
        [page[0], page[1], page[2], page[3], slot[0], slot[1]]
    }

    /// Rejects slot zero. Successful decoding does not establish tuple liveness.
    pub const fn from_le_bytes(bytes: [u8; Self::BYTE_LEN]) -> Option<Self> {
        Self::new(
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            u16::from_le_bytes([bytes[4], bytes[5]]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Ctid;

    #[test]
    fn first_page_and_first_slot_are_representable() {
        let ctid = Ctid::new(0, 1).unwrap();
        assert_eq!(ctid.page_id, 0);
        assert_eq!(ctid.slot_id.get(), 1);
        assert_eq!(ctid.to_le_bytes(), [0, 0, 0, 0, 1, 0]);
    }

    #[test]
    fn slot_zero_is_invalid() {
        assert_eq!(Ctid::new(42, 0), None);
        assert_eq!(Ctid::from_le_bytes([42, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn encoding_is_fixed_width_and_little_endian() {
        let ctid = Ctid::new(0x12345678, 0x9abc).unwrap();
        let bytes = [0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a];
        assert_eq!(ctid.to_le_bytes(), bytes);
        assert_eq!(Ctid::from_le_bytes(bytes), Some(ctid));
        assert_eq!(bytes.len(), Ctid::BYTE_LEN);
    }

    #[test]
    fn maximum_components_round_trip() {
        let ctid = Ctid::new(u32::MAX, u16::MAX).unwrap();
        assert_eq!(ctid.to_le_bytes(), [255; Ctid::BYTE_LEN]);
        assert_eq!(Ctid::from_le_bytes(ctid.to_le_bytes()), Some(ctid));
    }
}
