//! Stable, one-based slot IDs over a compactable, little-endian heap page.
use std::num::NonZeroU16;

use chilidb_storage::{PAGE_SIZE, Page, PageId};

use crate::{HeapError, Result};

const MAGIC: &[u8; 8] = b"CHILIHP1";
const HEADER_SIZE: usize = 24;
const SLOT_SIZE: usize = 4;
pub const MAX_TUPLE_SIZE: usize = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) owner: PageId,
    pub(crate) next: Option<PageId>,
    pub(crate) slots: u16,
}

fn u16_at(page: &Page, at: usize) -> u16 {
    u16::from_le_bytes([page[at], page[at + 1]])
}

fn u32_at(page: &Page, at: usize) -> u32 {
    u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]])
}

fn put_u16(page: &mut Page, at: usize, value: usize) {
    page[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes());
}

fn slot(page: &Page, index: usize) -> (usize, usize) {
    let at = HEADER_SIZE + index * SLOT_SIZE;
    (u16_at(page, at) as usize, u16_at(page, at + 2) as usize)
}

fn put_slot(page: &mut Page, index: usize, offset: usize, len: usize) {
    let at = HEADER_SIZE + index * SLOT_SIZE;
    put_u16(page, at, offset);
    put_u16(page, at + 2, len);
}

pub(crate) fn initialize(page: &mut Page, owner: PageId) {
    page.fill(0);
    page[..8].copy_from_slice(MAGIC);
    page[8..12].copy_from_slice(&owner.to_le_bytes());
    put_u16(page, 22, PAGE_SIZE);
}

// Validate before using any wire-provided index, including on read-only paths.
// Tracking occupied bytes keeps validation linear even for many tiny records.
fn validate(page: &Page) -> Result<(Header, usize)> {
    if &page[..8] != MAGIC {
        return Err(HeapError::Corrupt("invalid heap page magic"));
    }
    if page[16] > 1 || page[17..20] != [0; 3] {
        return Err(HeapError::Corrupt(
            "invalid heap page flags or reserved bytes",
        ));
    }
    let slots = u16_at(page, 20);
    let lower = HEADER_SIZE + slots as usize * SLOT_SIZE;
    let upper = u16_at(page, 22) as usize;
    if lower > upper || upper > PAGE_SIZE {
        return Err(HeapError::Corrupt("invalid heap page free space bounds"));
    }
    let mut occupied = [false; PAGE_SIZE];
    let mut used = 0;
    for index in 0..slots as usize {
        let (offset, len) = slot(page, index);
        if offset == 0 && len == 0 {
            continue;
        }
        if len == 0 || offset < upper || offset + len > PAGE_SIZE {
            return Err(HeapError::Corrupt("invalid heap slot bounds"));
        }
        for byte in &mut occupied[offset..offset + len] {
            if *byte {
                return Err(HeapError::Corrupt("overlapping heap slots"));
            }
            *byte = true;
        }
        used += len;
    }
    Ok((
        Header {
            owner: u32_at(page, 8),
            next: (page[16] == 1).then(|| u32_at(page, 12)),
            slots,
        },
        used,
    ))
}

pub(crate) fn header(page: &Page) -> Result<Header> {
    validate(page).map(|(header, _)| header)
}

pub(crate) fn set_next(page: &mut Page, next: Option<PageId>) -> Result<()> {
    validate(page)?;
    page[12..16].copy_from_slice(&next.unwrap_or(0).to_le_bytes());
    page[16] = u8::from(next.is_some());
    Ok(())
}

fn check_tuple(tuple: &[u8]) -> Result<()> {
    if tuple.is_empty() || tuple.len() > MAX_TUPLE_SIZE {
        return Err(HeapError::InvalidTupleSize {
            len: tuple.len(),
            max: MAX_TUPLE_SIZE,
        });
    }
    Ok(())
}

// Only called after validation and a successful space check. Rebuild into a
// temporary page so failed operations cannot partially modify the caller's page.
fn compact(page: &Page, header: Header, change: Option<(usize, Option<&[u8]>)>) -> Page {
    let mut result = [0; PAGE_SIZE];
    initialize(&mut result, header.owner);
    result[12..16].copy_from_slice(&header.next.unwrap_or(0).to_le_bytes());
    result[16] = u8::from(header.next.is_some());
    put_u16(&mut result, 20, header.slots as usize);
    let mut upper = PAGE_SIZE;
    for index in 0..header.slots as usize {
        let tuple = match change {
            Some((changed, tuple)) if changed == index => tuple,
            _ => {
                let (offset, len) = slot(page, index);
                (len != 0).then(|| &page[offset..offset + len])
            }
        };
        if let Some(tuple) = tuple {
            upper -= tuple.len();
            result[upper..upper + tuple.len()].copy_from_slice(tuple);
            put_slot(&mut result, index, upper, tuple.len());
        }
    }
    put_u16(&mut result, 22, upper);
    result
}

pub(crate) fn insert(page: &mut Page, tuple: &[u8]) -> Result<Option<NonZeroU16>> {
    check_tuple(tuple)?;
    let (header, used) = validate(page)?;
    let new_slots = header.slots as usize + 1;
    if HEADER_SIZE + new_slots * SLOT_SIZE + used + tuple.len() > PAGE_SIZE {
        return Ok(None);
    }
    let mut result = compact(page, header, None);
    let upper = PAGE_SIZE - used - tuple.len();
    result[upper..upper + tuple.len()].copy_from_slice(tuple);
    put_slot(&mut result, header.slots as usize, upper, tuple.len());
    put_u16(&mut result, 20, new_slots);
    put_u16(&mut result, 22, upper);
    *page = result;
    Ok(NonZeroU16::new(new_slots as u16))
}

pub(crate) fn get(page: &Page, id: NonZeroU16) -> Result<Option<&[u8]>> {
    let (header, _) = validate(page)?;
    if id.get() > header.slots {
        return Ok(None);
    }
    let (offset, len) = slot(page, id.get() as usize - 1);
    Ok((len != 0).then(|| &page[offset..offset + len]))
}

pub(crate) fn records(
    page: &Page,
    limit: u16,
) -> Result<impl Iterator<Item = (NonZeroU16, &[u8])> + '_> {
    let (header, _) = validate(page)?;
    if limit > header.slots {
        return Err(HeapError::Corrupt("scan limit exceeds slot directory"));
    }
    Ok((0..usize::from(limit)).filter_map(move |index| {
        let (offset, len) = slot(page, index);
        (len != 0).then(|| {
            (
                NonZeroU16::new(index as u16 + 1).expect("one-based slot"),
                &page[offset..offset + len],
            )
        })
    }))
}

pub(crate) fn delete(page: &mut Page, id: NonZeroU16) -> Result<bool> {
    let (header, _) = validate(page)?;
    let index = id.get() as usize - 1;
    if id.get() > header.slots || slot(page, index).1 == 0 {
        return Ok(false);
    }
    *page = compact(page, header, Some((index, None)));
    Ok(true)
}

pub(crate) fn replace(page: &mut Page, id: NonZeroU16, tuple: &[u8]) -> Result<bool> {
    check_tuple(tuple)?;
    let (header, used) = validate(page)?;
    let index = id.get() as usize - 1;
    if id.get() > header.slots {
        return Ok(false);
    }
    let old_len = slot(page, index).1;
    if old_len == 0
        || HEADER_SIZE + header.slots as usize * SLOT_SIZE + used - old_len + tuple.len()
            > PAGE_SIZE
    {
        return Ok(false);
    }
    *page = compact(page, header, Some((index, Some(tuple))));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Page {
        let mut page = [0xff; PAGE_SIZE];
        initialize(&mut page, 0x12345678);
        page
    }

    fn id(value: u16) -> NonZeroU16 {
        NonZeroU16::new(value).unwrap()
    }

    #[test]
    fn record_iteration_is_bounded_and_skips_tombstones() {
        let mut page = fresh();
        insert(&mut page, b"a").unwrap();
        insert(&mut page, b"b").unwrap();
        insert(&mut page, b"c").unwrap();
        delete(&mut page, id(1)).unwrap();
        assert_eq!(
            records(&page, 2).unwrap().collect::<Vec<_>>(),
            vec![(id(2), &b"b"[..])]
        );
        assert_eq!(records(&page, 0).unwrap().count(), 0);
        assert!(records(&page, 4).is_err());
    }

    #[test]
    fn header_wire_format() {
        let mut page = fresh();
        assert_eq!(&page[..8], b"CHILIHP1");
        assert_eq!(&page[8..12], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(&page[12..22], &[0; 10]);
        assert_eq!(&page[22..24], &[0, 0x20]);
        assert!(page[24..].iter().all(|&byte| byte == 0));
        assert_eq!(
            header(&page).unwrap(),
            Header {
                owner: 0x12345678,
                next: None,
                slots: 0
            }
        );
        set_next(&mut page, Some(0x87654321)).unwrap();
        assert_eq!(&page[12..17], &[0x21, 0x43, 0x65, 0x87, 1]);
        assert_eq!(header(&page).unwrap().next, Some(0x87654321));
        set_next(&mut page, Some(0)).unwrap();
        assert_eq!(header(&page).unwrap().next, Some(0));
        set_next(&mut page, None).unwrap();
        assert_eq!(&page[12..17], &[0; 5]);
        insert(&mut page, b"abc").unwrap();
        assert_eq!(&page[20..28], &[1, 0, 0xfd, 0x1f, 0xfd, 0x1f, 3, 0]);
    }

    #[test]
    fn variable_lengths_capacity_and_atomic_no_fit() {
        let mut page = fresh();
        let a = insert(&mut page, b"a").unwrap().unwrap();
        let b = insert(&mut page, &[2; 4000]).unwrap().unwrap();
        assert_eq!(get(&page, a).unwrap(), Some(&b"a"[..]));
        assert_eq!(get(&page, b).unwrap(), Some(&[2; 4000][..]));
        let before = page;
        assert_eq!(insert(&mut page, &[3; 4200]).unwrap(), None);
        assert_eq!(page, before);
        assert!(!replace(&mut page, a, &[3; 4200]).unwrap());
        assert_eq!(page, before);
        assert!(!replace(&mut page, id(3), b"missing").unwrap());
        assert!(!delete(&mut page, id(3)).unwrap());
        assert_eq!(page, before);
    }

    #[test]
    fn delete_compacts_without_reusing_slots() {
        let mut page = fresh();
        set_next(&mut page, Some(42)).unwrap();
        let a = insert(&mut page, &[1; 100]).unwrap().unwrap();
        let b = insert(&mut page, &[2; 200]).unwrap().unwrap();
        assert!(delete(&mut page, a).unwrap());
        assert_eq!(u16_at(&page, 22) as usize, PAGE_SIZE - 200);
        assert_eq!(slot(&page, 0), (0, 0));
        assert_eq!(get(&page, a).unwrap(), None);
        let before = page;
        assert!(!delete(&mut page, a).unwrap());
        assert!(!replace(&mut page, a, b"dead").unwrap());
        assert_eq!(page, before);
        assert_eq!(insert(&mut page, b"new").unwrap(), Some(id(3)));
        assert_eq!(get(&page, b).unwrap(), Some(&[2; 200][..]));
        assert_eq!(header(&page).unwrap().next, Some(42));
    }

    #[test]
    fn replacement_grows_and_shrinks_with_stable_ids() {
        let mut page = fresh();
        let a = insert(&mut page, b"abc").unwrap().unwrap();
        let b = insert(&mut page, b"untouched").unwrap().unwrap();
        for bytes in [&[7; 1000][..], &b"x"[..]] {
            assert!(replace(&mut page, a, bytes).unwrap());
            assert_eq!(get(&page, a).unwrap(), Some(bytes));
            assert_eq!(get(&page, b).unwrap(), Some(&b"untouched"[..]));
            assert_eq!(header(&page).unwrap().slots, 2);
            assert_eq!(u16_at(&page, 22) as usize, PAGE_SIZE - bytes.len() - 9);
        }
    }

    #[test]
    fn exact_maximum_record_and_invalid_sizes() {
        let mut page = fresh();
        let tuple = [9; MAX_TUPLE_SIZE];
        assert_eq!(insert(&mut page, &tuple).unwrap(), Some(id(1)));
        assert_eq!(get(&page, id(1)).unwrap(), Some(&tuple[..]));
        let before = page;
        assert_eq!(insert(&mut page, b"x").unwrap(), None);
        for invalid in [&[][..], &[0; MAX_TUPLE_SIZE + 1][..]] {
            assert!(matches!(
                insert(&mut page, invalid),
                Err(HeapError::InvalidTupleSize { .. })
            ));
            assert!(matches!(
                replace(&mut page, id(1), invalid),
                Err(HeapError::InvalidTupleSize { .. })
            ));
        }
        assert_eq!(page, before);
    }

    #[test]
    fn slot_directory_eventually_exhausts_without_aba() {
        let mut page = fresh();
        let capacity = (PAGE_SIZE - HEADER_SIZE - 1) / SLOT_SIZE;
        for expected in 1..=capacity {
            let slot_id = insert(&mut page, b"x").unwrap().unwrap();
            assert_eq!(slot_id.get() as usize, expected);
            assert!(delete(&mut page, slot_id).unwrap());
        }
        let before = page;
        assert_eq!(insert(&mut page, b"x").unwrap(), None);
        assert_eq!(page, before);
        assert_eq!(header(&page).unwrap().slots as usize, capacity);
        for old in 1..=capacity {
            assert_eq!(get(&page, id(old as u16)).unwrap(), None);
        }
    }

    #[test]
    fn corruption_is_rejected_on_all_paths_without_mutation() {
        let mut valid = fresh();
        insert(&mut valid, b"abc").unwrap();
        insert(&mut valid, b"def").unwrap();
        let mutations: &[(usize, &[u8])] = &[
            (0, b"X"),
            (16, &[2]),
            (17, &[1]),
            (18, &[1]),
            (19, &[1]),
            (20, &[0xff, 0xff]),
            (22, &[0, 0]),
            (22, &[1, 0x20]),
            (24, &[1, 0]),
            (24, &[0xff, 0x1f]),
            (26, &[0, 0]),
            (26, &[0xff, 0xff]),
            (28, &[0xfd, 0x1f]),
        ];
        for &(offset, bytes) in mutations {
            let mut page = valid;
            page[offset..offset + bytes.len()].copy_from_slice(bytes);
            let before = page;
            assert!(
                matches!(header(&page), Err(HeapError::Corrupt(_))),
                "offset {offset}"
            );
            assert!(get(&page, id(1)).is_err());
            assert!(insert(&mut page, b"x").is_err());
            assert!(delete(&mut page, id(1)).is_err());
            assert!(replace(&mut page, id(1), b"x").is_err());
            assert!(set_next(&mut page, Some(1)).is_err());
            assert_eq!(page, before);
        }
    }

    #[test]
    fn insertion_compacts_valid_fragmented_payload() {
        let mut page = fresh();
        put_u16(&mut page, 20, 1);
        put_u16(&mut page, 22, 28);
        put_slot(&mut page, 0, 100, 3);
        page[100..103].copy_from_slice(b"old");
        assert_eq!(insert(&mut page, &[7; 8000]).unwrap(), Some(id(2)));
        assert_eq!(get(&page, id(1)).unwrap(), Some(&b"old"[..]));
        assert_eq!(get(&page, id(2)).unwrap(), Some(&[7; 8000][..]));
    }
}
