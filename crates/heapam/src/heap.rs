use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

use chilidb_storage::{BufferPool, Page, PageId};

use crate::{Ctid, HeapError, MAX_TUPLE_SIZE, Result, page};

struct State {
    pages: Vec<PageId>,
    indices: HashMap<PageId, usize>,
}

/// Open once per heap and share the handle with Arc. Independent handles for the
/// same root do not coordinate their cached page chains. Existing records never
/// change CTID through compaction; deleted slot IDs are never reused.
pub struct HeapTable {
    pool: Arc<BufferPool>,
    head: PageId,
    state: Mutex<State>,
}

impl fmt::Debug for HeapTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeapTable")
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

impl HeapTable {
    pub fn create(pool: Arc<BufferPool>) -> Result<Self> {
        let head = {
            let pin = pool.new_page()?;
            let head = pin.page_id();
            pin.write(|bytes| page::initialize(bytes, head))?;
            head
        };
        Ok(Self {
            pool,
            head,
            state: Mutex::new(State {
                pages: vec![head],
                indices: HashMap::from([(head, 0)]),
            }),
        })
    }

    /// Validate and cache the complete chain, without retaining any buffer pins.
    pub fn open(pool: Arc<BufferPool>, head: PageId) -> Result<Self> {
        let mut pages = Vec::new();
        let mut indices = HashMap::new();
        let mut next = Some(head);
        while let Some(id) = next {
            if indices.insert(id, pages.len()).is_some() {
                return Err(HeapError::Corrupt("cycle in page chain"));
            }
            let pin = pool.pin(id)?;
            let header = pin.read(page::header)??;
            if header.owner != head {
                return Err(HeapError::Corrupt("page belongs to another heap"));
            }
            pages.push(id);
            next = header.next;
        }
        Ok(Self {
            pool,
            head,
            state: Mutex::new(State { pages, indices }),
        })
    }

    pub fn head_page_id(&self) -> PageId {
        self.head
    }

    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| HeapError::Poisoned)
    }

    fn checked_header(&self, bytes: &Page, index: usize, state: &State) -> Result<page::Header> {
        let header = page::header(bytes)?;
        if header.owner != self.head {
            return Err(HeapError::Corrupt("page belongs to another heap"));
        }
        if header.next != state.pages.get(index + 1).copied() {
            return Err(HeapError::Corrupt(
                "page chain changed outside this heap handle",
            ));
        }
        Ok(header)
    }

    fn validate_tuple(bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() || bytes.len() > MAX_TUPLE_SIZE {
            return Err(HeapError::InvalidTupleSize {
                len: bytes.len(),
                max: MAX_TUPLE_SIZE,
            });
        }
        Ok(())
    }

    /// Append into the tail page or link a new one. Oversized records are rejected
    /// before allocation; there are no overflow pages or free-space maps.
    pub fn insert(&self, bytes: &[u8]) -> Result<Ctid> {
        Self::validate_tuple(bytes)?;
        let mut state = self.state()?;
        let index = state.pages.len() - 1;
        let tail = state.pages[index];
        {
            let pin = self.pool.pin(tail)?;
            let slot = pin.write(|page| {
                self.checked_header(page, index, &state)?;
                page::insert(page, bytes)
            })??;
            if let Some(slot_id) = slot {
                return Ok(Ctid {
                    page_id: tail,
                    slot_id,
                });
            }
        }
        // Release the old page before allocation so a one-frame pool is usable.
        let (new_page, slot_id) = {
            let pin = self.pool.new_page()?;
            let new_page = pin.page_id();
            let slot_id = pin.write(|page| {
                page::initialize(page, self.head);
                page::insert(page, bytes)?
                    .ok_or(HeapError::Corrupt("valid tuple cannot fit on a new page"))
            })??;
            (new_page, slot_id)
        };
        {
            let pin = self.pool.pin(tail)?;
            pin.write(|page| {
                self.checked_header(page, index, &state)?;
                page::set_next(page, Some(new_page))
            })??;
        }
        // Publish membership only after linking. A failure before this point can
        // leak an unlinked allocated page, but does not expose its tuple via CTID.
        let index = state.pages.len();
        state.pages.push(new_page);
        state.indices.insert(new_page, index);
        Ok(Ctid {
            page_id: new_page,
            slot_id,
        })
    }

    pub fn get(&self, ctid: Ctid) -> Result<Option<Vec<u8>>> {
        let state = self.state()?;
        let index = *state
            .indices
            .get(&ctid.page_id)
            .ok_or(HeapError::InvalidCtid(ctid))?;
        self.pool.pin(ctid.page_id)?.read(|page| {
            self.checked_header(page, index, &state)?;
            Ok(page::get(page, ctid.slot_id)?.map(<[u8]>::to_vec))
        })?
    }

    pub fn delete(&self, ctid: Ctid) -> Result<bool> {
        let state = self.state()?;
        let index = *state
            .indices
            .get(&ctid.page_id)
            .ok_or(HeapError::InvalidCtid(ctid))?;
        self.pool.pin(ctid.page_id)?.write(|page| {
            self.checked_header(page, index, &state)?;
            page::delete(page, ctid.slot_id)
        })?
    }

    /// Replace at the same CTID. NoSpace leaves the record unchanged; relocation
    /// and MVCC version creation require a higher-level transaction protocol.
    pub fn replace(&self, ctid: Ctid, bytes: &[u8]) -> Result<bool> {
        Self::validate_tuple(bytes)?;
        let state = self.state()?;
        let index = *state
            .indices
            .get(&ctid.page_id)
            .ok_or(HeapError::InvalidCtid(ctid))?;
        self.pool.pin(ctid.page_id)?.write(|page| {
            self.checked_header(page, index, &state)?;
            if page::get(page, ctid.slot_id)?.is_none() {
                return Ok(false);
            }
            if !page::replace(page, ctid.slot_id, bytes)? {
                return Err(HeapError::NoSpace);
            }
            Ok(true)
        })?
    }

    /// Capture page/slot upper bounds, excluding subsequently inserted slots.
    /// Each page's records are copied when visited, not at scan creation. This is
    /// not an MVCC snapshot: later replacements/deletions can affect unread pages.
    pub fn scan(&self) -> Result<HeapScan<'_>> {
        let state = self.state()?;
        let mut pages = Vec::with_capacity(state.pages.len());
        for (index, &id) in state.pages.iter().enumerate() {
            let pin = self.pool.pin(id)?;
            let header = pin.read(|page| self.checked_header(page, index, &state))??;
            pages.push((id, header.slots));
        }
        Ok(HeapScan {
            heap: self,
            pages,
            index: 0,
            rows: VecDeque::new(),
            failed: false,
        })
    }

    fn read_scan_page(&self, id: PageId, slots: u16) -> Result<VecDeque<(Ctid, Vec<u8>)>> {
        let state = self.state()?;
        let index = *state
            .indices
            .get(&id)
            .ok_or(HeapError::Corrupt("scan page disappeared"))?;
        self.pool.pin(id)?.read(|page| {
            let header = self.checked_header(page, index, &state)?;
            if header.slots < slots {
                return Err(HeapError::Corrupt("slot directory shrank"));
            }
            let mut rows = VecDeque::new();
            for (slot_id, bytes) in page::records(page, slots)? {
                rows.push_back((
                    Ctid {
                        page_id: id,
                        slot_id,
                    },
                    bytes.to_vec(),
                ));
            }
            Ok(rows)
        })?
    }
}

/// At most one page's owned records are buffered. No latch, pin, or heap mutex
/// remains held across next calls. An error terminates the scan.
pub struct HeapScan<'a> {
    heap: &'a HeapTable,
    pages: Vec<(PageId, u16)>,
    index: usize,
    rows: VecDeque<(Ctid, Vec<u8>)>,
    failed: bool,
}
impl Iterator for HeapScan<'_> {
    type Item = Result<(Ctid, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            if let Some(row) = self.rows.pop_front() {
                return Some(Ok(row));
            }
            let &(id, slots) = self.pages.get(self.index)?;
            self.index += 1;
            match self.heap.read_scan_page(id, slots) {
                Ok(rows) => self.rows = rows,
                Err(error) => {
                    self.failed = true;
                    return Some(Err(error));
                }
            }
        }
    }
}
impl std::iter::FusedIterator for HeapScan<'_> {}
