//! Buffer pool benchmark following BusTub's `bpm_bench` workload.
//!
//! Scan threads repeatedly read disjoint contiguous page ranges; get threads
//! update Zipf-selected pages, each owning pages congruent to its thread ID.
//! Each benchmark measures aggregate throughput of one thread kind while the
//! other kind runs concurrently.
//! Every access validates page contents, and a final pass checks all updates.

use std::{
    collections::HashMap,
    hint::black_box,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use chilidb_storage::{BufferPool, MemoryPageStore, Page, PageId, PageStore};
use criterion::{
    BenchmarkId, Criterion, SamplingMode, Throughput, criterion_group, criterion_main,
};

const SCAN_THREADS: usize = 8;
const GET_THREADS: usize = 8;
const BPM_SIZE: usize = 64;
const DB_SIZE: usize = 6400;
const ZIPF_THETA: f64 = 0.8;
const SEED: u64 = 15445;

/// Memory store with BusTub's simulated latency: 1 ms per random access, or
/// 100 us within the 4-page block or 3 pages after one of the last 4 accesses.
struct LatencyStore {
    inner: MemoryPageStore,
    enabled: AtomicBool,
    recent: Mutex<([PageId; 4], usize)>,
}

impl LatencyStore {
    fn process(&self, id: PageId) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let near = self.recent.lock().unwrap().0.iter().any(|&recent| {
            recent & !3 == id & !3 || (recent..=recent.saturating_add(3)).contains(&id)
        });
        thread::sleep(Duration::from_micros(if near { 100 } else { 1000 }));
    }

    fn post_process(&self, id: PageId) {
        if self.enabled.load(Ordering::Relaxed) {
            let (recent, next) = &mut *self.recent.lock().unwrap();
            recent[*next] = id;
            *next = (*next + 1) % recent.len();
        }
    }
}

impl PageStore for LatencyStore {
    fn allocate_page(&self) -> io::Result<PageId> {
        self.inner.allocate_page()
    }

    fn read_page(&self, id: PageId, data: &mut Page) -> io::Result<()> {
        self.process(id);
        self.inner.read_page(id, data)?;
        self.post_process(id);
        Ok(())
    }

    fn write_page(&self, id: PageId, data: &Page) -> io::Result<()> {
        self.process(id);
        self.inner.write_page(id, data)?;
        self.post_process(id);
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
}

// Page layout: seed (u64), page index (u64), then data[seed % 4000] = seed % 256.
const HEADER: usize = 16;

fn modify(page: &mut Page, index: usize, seed: u64) {
    page[..8].copy_from_slice(&seed.to_le_bytes());
    page[8..16].copy_from_slice(&(index as u64).to_le_bytes());
    page[HEADER + (seed % 4000) as usize] = seed as u8;
}

/// Validate the page's self-consistency and return its seed.
fn check(page: &Page, index: usize) -> u64 {
    let seed = u64::from_le_bytes(page[..8].try_into().unwrap());
    let stored = u64::from_le_bytes(page[8..16].try_into().unwrap());
    assert_eq!(stored, index as u64, "page index mismatch");
    assert_eq!(
        page[HEADER + (seed % 4000) as usize],
        seed as u8,
        "page {index} data does not match seed {seed}"
    );
    seed
}

/// SplitMix64: small, seedable, and good enough for workload generation.
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Zipf over 0..n where rank 0 is hottest (Gray et al., as used by YCSB).
struct Zipf {
    n: usize,
    theta: f64,
    zetan: f64,
    eta: f64,
}

impl Zipf {
    fn new(n: usize, theta: f64) -> Self {
        let zeta = |n: usize| (1..=n).map(|i| 1.0 / (i as f64).powf(theta)).sum::<f64>();
        let zetan = zeta(n);
        let eta = (1.0 - (2.0 / n as f64).powf(1.0 - theta)) / (1.0 - zeta(2) / zetan);
        Self {
            n,
            theta,
            zetan,
            eta,
        }
    }

    fn sample(&self, rng: &mut Rng) -> usize {
        let u = rng.next_f64();
        let uz = u * self.zetan;
        if uz < 1.0 {
            return 0;
        }
        if uz < 1.0 + 0.5f64.powf(self.theta) {
            return 1;
        }
        let rank = self.n as f64 * (self.eta * u - self.eta + 1.0).powf(1.0 / (1.0 - self.theta));
        (rank as usize).min(self.n - 1)
    }
}

struct GetState {
    rng: Rng,
    seeds: HashMap<usize, u64>,
}

struct Workload {
    pool: BufferPool,
    pages: Vec<PageId>,
    zipf: Zipf,
    scans: Vec<AtomicUsize>,
    gets: Vec<Mutex<GetState>>,
}

#[derive(Clone, Copy)]
enum Kind {
    Scan,
    Get,
}

impl Workload {
    fn new(latency: bool) -> Self {
        let store = Arc::new(LatencyStore {
            inner: MemoryPageStore::new(),
            enabled: AtomicBool::new(false),
            recent: Mutex::new(([0; 4], 0)),
        });
        let pool = BufferPool::new(BPM_SIZE, store.clone()).unwrap();
        let pages = (0..DB_SIZE)
            .map(|index| {
                let pin = pool.new_page().unwrap();
                pin.write(|page| modify(page, index, 0)).unwrap();
                pin.page_id()
            })
            .collect();
        store.enabled.store(latency, Ordering::Relaxed);
        let scans = (0..SCAN_THREADS)
            .map(|tid| AtomicUsize::new(DB_SIZE * tid / SCAN_THREADS))
            .collect();
        let gets = (0..GET_THREADS as u64)
            .map(|tid| {
                Mutex::new(GetState {
                    rng: Rng(SEED ^ (tid + 1).wrapping_mul(0x2545_f491_4f6c_dd1d)),
                    seeds: HashMap::new(),
                })
            })
            .collect();
        Self {
            pool,
            pages,
            zipf: Zipf::new(DB_SIZE, ZIPF_THETA),
            scans,
            gets,
        }
    }

    /// Continue the thread's sequential scan, persisting its position across runs.
    fn scan(&self, tid: usize, more: &dyn Fn() -> bool) {
        let range = DB_SIZE * tid / SCAN_THREADS..DB_SIZE * (tid + 1) / SCAN_THREADS;
        let mut index = self.scans[tid].load(Ordering::Relaxed);
        while more() {
            let pin = self.pool.pin(self.pages[index]).unwrap();
            black_box(pin.read(|page| check(page, index)).unwrap());
            index = if index + 1 == range.end {
                range.start
            } else {
                index + 1
            };
        }
        self.scans[tid].store(index, Ordering::Relaxed);
    }

    fn get(&self, tid: usize, more: &dyn Fn() -> bool) {
        let GetState { rng, seeds } = &mut *self.gets[tid].lock().unwrap();
        while more() {
            // DB_SIZE is a multiple of GET_THREADS, so ownership is disjoint.
            let index = self.zipf.sample(rng) / GET_THREADS * GET_THREADS + tid;
            let seed = seeds.entry(index).or_insert(0);
            let pin = self.pool.pin(self.pages[index]).unwrap();
            pin.write(|page| {
                assert_eq!(check(page, index), *seed, "lost update on page {index}");
                *seed += 1;
                modify(page, index, *seed);
            })
            .unwrap();
        }
    }

    fn run(&self, kind: Kind, tid: usize, more: &dyn Fn() -> bool) {
        match kind {
            Kind::Scan => self.scan(tid, more),
            Kind::Get => self.get(tid, more),
        }
    }

    /// Time `iters` operations shared by the `measured` threads while the
    /// other kind runs concurrently.
    fn measure(&self, measured: Kind, iters: u64) -> Duration {
        let (threads, background, background_threads) = match measured {
            Kind::Scan => (SCAN_THREADS, Kind::Get, GET_THREADS),
            Kind::Get => (GET_THREADS, Kind::Scan, SCAN_THREADS),
        };
        let stop = AtomicBool::new(false);
        let remaining = AtomicU64::new(iters);
        let running = || !stop.load(Ordering::Relaxed);
        let budget = || {
            remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok()
        };
        thread::scope(|s| {
            for tid in 0..background_threads {
                s.spawn(move || self.run(background, tid, &running));
            }
            let start = Instant::now();
            let handles: Vec<_> = (0..threads)
                .map(|tid| s.spawn(move || self.run(measured, tid, &budget)))
                .collect();
            handles.into_iter().for_each(|h| h.join().unwrap());
            let elapsed = start.elapsed();
            stop.store(true, Ordering::Relaxed);
            elapsed
        })
    }

    fn verify(&self) {
        for get in &self.gets {
            for (&index, &seed) in &get.lock().unwrap().seeds {
                let pin = self.pool.pin(self.pages[index]).unwrap();
                assert_eq!(pin.read(|page| check(page, index)).unwrap(), seed);
            }
        }
    }
}

fn bpm_bench(c: &mut Criterion) {
    assert!(SCAN_THREADS + GET_THREADS <= BPM_SIZE && DB_SIZE.is_multiple_of(GET_THREADS));
    let mut group = c.benchmark_group("bpm_bench");
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .throughput(Throughput::Elements(1));
    for latency in [false, true] {
        let workload = Workload::new(latency);
        let name = if latency { "latency" } else { "memory" };
        for (kind, id) in [(Kind::Scan, "scan"), (Kind::Get, "get")] {
            group.bench_function(BenchmarkId::new(id, name), |b| {
                b.iter_custom(|iters| workload.measure(kind, iters))
            });
        }
        workload.verify();
    }
    group.finish();
}

criterion_group!(benches, bpm_bench);
criterion_main!(benches);
