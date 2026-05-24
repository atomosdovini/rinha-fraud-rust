use memmap2::MmapOptions;
use std::fs::File;

pub const DIMS: usize = 14;
const BLOCK: usize = 8;
const MAGIC: u64 = 0x3149564936324852;
const VER: u32 = 1;

#[repr(C)]
struct FileHeader {
    magic: u64,
    version: u32,
    n: u32,
    k: u32,
    total_blocks: u32,
    block_size: u32,
    dims: u32,
    reserved: [u32; 8],
}

pub struct IvfIndex {
    _mmap: memmap2::Mmap,
    raw: *const u8,
    k: usize,
    centroids: *const i16,
    bmin: *const i16,
    bmax: *const i16,
    offsets: *const u32,
    counts: *const u32,
    labels: *const u8,
    vecs: *const i16,
    csoa: Vec<i16>, // [dim * K + c] transposed centroid layout for AVX2
}

unsafe impl Send for IvfIndex {}
unsafe impl Sync for IvfIndex {}

fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

struct Sections {
    centroids: usize,
    bbox_min: usize,
    bbox_max: usize,
    offsets: usize,
    counts: usize,
    labels: usize,
    blocks: usize,
}

fn compute_sections(k: usize, total_blocks: usize) -> Sections {
    let mut p = std::mem::size_of::<FileHeader>();
    let centroids = p; p += k * DIMS * 2;
    let bbox_min  = p; p += k * DIMS * 2;
    let bbox_max  = p; p += k * DIMS * 2;
    p = align_up(p, 4);
    let offsets = p; p += (k + 1) * 4;
    let counts  = p; p += k * 4;
    let labels  = p; p += total_blocks * BLOCK;
    p = align_up(p, 2);
    let blocks = p; let _ = p + total_blocks * DIMS * BLOCK * 2;
    Sections { centroids, bbox_min, bbox_max, offsets, counts, labels, blocks }
}

pub struct QueryTrace {
    pub final_worst: u64,
    pub fraud: u8,
}

impl IvfIndex {
    pub fn open(path: &str) -> Self {
        let file = File::open(path).expect("open index");
        let mmap = unsafe { MmapOptions::new().map(&file).expect("mmap index") };
        let raw = mmap.as_ptr();

        let hdr = unsafe { &*(raw as *const FileHeader) };
        assert_eq!(hdr.magic, MAGIC, "bad magic");
        assert_eq!(hdr.version, VER, "bad version");
        assert_eq!(hdr.dims, DIMS as u32, "bad dims");
        assert_eq!(hdr.block_size, BLOCK as u32, "bad block_size");

        let k = hdr.k as usize;
        let total_blocks = hdr.total_blocks as usize;
        let sec = compute_sections(k, total_blocks);

        let centroids = unsafe { raw.add(sec.centroids) as *const i16 };
        let bmin      = unsafe { raw.add(sec.bbox_min)  as *const i16 };
        let bmax      = unsafe { raw.add(sec.bbox_max)  as *const i16 };
        let offsets   = unsafe { raw.add(sec.offsets)   as *const u32 };
        let counts    = unsafe { raw.add(sec.counts)    as *const u32 };
        let labels    = unsafe { raw.add(sec.labels) };
        let vecs      = unsafe { raw.add(sec.blocks)    as *const i16 };

        // Transpose centroids into SOA: csoa[d * K + c] = centroids[c * DIMS + d]
        let mut csoa = vec![0i16; DIMS * k];
        unsafe {
            for c in 0..k {
                let src = centroids.add(c * DIMS);
                for d in 0..DIMS {
                    csoa[d * k + c] = *src.add(d);
                }
            }
        }

        // Warm up pages (MADV_WILLNEED already set by mmap, but touch first page of each section)
        let _ = unsafe { std::ptr::read_volatile(raw) };

        IvfIndex { _mmap: mmap, raw, k, centroids, bmin, bmax, offsets, counts, labels, vecs, csoa }
    }

    /// Scalar L2 distance from query to centroid c.
    #[inline]
    fn centroid_dist_scalar(&self, q: *const i16, c: usize) -> u64 {
        unsafe {
            let b = self.centroids.add(c * DIMS);
            let mut acc = 0u64;
            for d in 0..DIMS {
                let e = *q.add(d) as i64 - *b.add(d) as i64;
                acc += (e * e) as u64;
            }
            acc
        }
    }

    /// Bounding-box lower bound for pruning.
    #[inline]
    fn bbox_lb(&self, q: *const i16, c: usize) -> u64 {
        unsafe {
            let lo = self.bmin.add(c * DIMS);
            let hi = self.bmax.add(c * DIMS);
            let mut acc = 0u64;
            for d in 0..DIMS {
                let qd = *q.add(d) as i64;
                let e: i64 = if qd < *lo.add(d) as i64 {
                    *lo.add(d) as i64 - qd
                } else if qd > *hi.add(d) as i64 {
                    qd - *hi.add(d) as i64
                } else {
                    0
                };
                acc += (e * e) as u64;
            }
            acc
        }
    }

    /// Compute L2 distances from q to 8 consecutive centroids using AVX2.
    /// Returns [dist0..dist7] packed into a [u32; 8].
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn avx2_dist8(&self, q: *const i16, c_base: usize) -> [u32; 8] {
        use std::arch::x86_64::*;
        let k = self.k;
        let csoa = self.csoa.as_ptr();
        let mut acc = _mm256_setzero_si256();
        let mut d = 0;
        while d < DIMS {
            let r0 = _mm_loadu_si128(csoa.add(d * k + c_base) as *const __m128i);
            let r1 = _mm_loadu_si128(csoa.add((d + 1) * k + c_base) as *const __m128i);
            let diff0 = _mm_sub_epi16(_mm_set1_epi16(*q.add(d)), r0);
            let diff1 = _mm_sub_epi16(_mm_set1_epi16(*q.add(d + 1)), r1);
            let lo = _mm_unpacklo_epi16(diff0, diff1);
            let hi = _mm_unpackhi_epi16(diff0, diff1);
            let pairs = _mm256_set_m128i(hi, lo);
            acc = _mm256_add_epi32(acc, _mm256_madd_epi16(pairs, pairs));
            d += 2;
        }
        let mut out = [0u32; 8];
        _mm256_storeu_si256(out.as_mut_ptr() as *mut __m256i, acc);
        out
    }

    /// Score 8 vectors in a block (interleaved layout: vblock[d * BLOCK + lane]) using AVX2.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn score_block8(vblock: *const i16, lbls: *const u8, q: *const i16, heap: &mut KnnHeap) {
        use std::arch::x86_64::*;
        let mut acc = _mm256_setzero_si256();
        let mut d = 0;
        while d < DIMS {
            let ref0 = _mm_loadu_si128(vblock.add(d * BLOCK) as *const __m128i);
            let ref1 = _mm_loadu_si128(vblock.add((d + 1) * BLOCK) as *const __m128i);
            let diff0 = _mm_sub_epi16(_mm_set1_epi16(*q.add(d)), ref0);
            let diff1 = _mm_sub_epi16(_mm_set1_epi16(*q.add(d + 1)), ref1);
            let lo = _mm_unpacklo_epi16(diff0, diff1);
            let hi = _mm_unpackhi_epi16(diff0, diff1);
            let pairs = _mm256_set_m128i(hi, lo);
            acc = _mm256_add_epi32(acc, _mm256_madd_epi16(pairs, pairs));
            d += 2;
        }
        let mut dist = [0u32; 8];
        _mm256_storeu_si256(dist.as_mut_ptr() as *mut __m256i, acc);
        let mut gate = heap.gate();
        for lane in 0..8usize {
            let dv = dist[lane] as u64;
            if dv < gate {
                heap.insert(dv, *lbls.add(lane));
                gate = heap.gate();
            }
        }
    }

    fn probe_cluster(&self, c: usize, q: *const i16, heap: &mut KnnHeap) {
        unsafe {
            let blk_start = *self.offsets.add(c) as usize;
            let cnt       = *self.counts.add(c) as usize;
            if cnt == 0 { return; }
            let nblocks = (cnt + BLOCK - 1) / BLOCK;
            for b in 0..nblocks {
                let blk_id  = blk_start + b;
                let valid   = (cnt - b * BLOCK).min(BLOCK);
                let vblock  = self.vecs.add(blk_id * DIMS * BLOCK);
                let lbls    = self.labels.add(blk_id * BLOCK);
                if valid == BLOCK {
                    #[cfg(target_arch = "x86_64")]
                    Self::score_block8(vblock, lbls, q, heap);
                    #[cfg(not(target_arch = "x86_64"))]
                    self.score_block_scalar(vblock, lbls, valid, q, heap);
                } else {
                    for lane in 0..valid {
                        let mut acc = 0u64;
                        let gate = heap.gate();
                        for d in 0..DIMS {
                            let e = *q.add(d) as i64 - *vblock.add(d * BLOCK + lane) as i64;
                            acc += (e * e) as u64;
                            if acc >= gate { break; }
                        }
                        heap.insert(acc, *lbls.add(lane));
                    }
                }
            }
        }
    }

    /// Find nearest centroid, probe it. Returns (fraud_count, worst_dist_in_heap).
    pub fn query_top1(&self, q: *const i16) -> QueryTrace {
        let k = self.k;
        let mut best_c = 0usize;
        let mut best_d = u64::MAX;

        #[cfg(target_arch = "x86_64")]
        unsafe {
            let mut c = 0usize;
            while c + 8 <= k {
                let dists = self.avx2_dist8(q, c);
                for lane in 0..8 {
                    let d = dists[lane] as u64;
                    if d < best_d { best_d = d; best_c = c + lane; }
                }
                c += 8;
            }
            while c < k {
                let d = self.centroid_dist_scalar(q, c);
                if d < best_d { best_d = d; best_c = c; }
                c += 1;
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        for c in 0..k {
            let d = self.centroid_dist_scalar(q, c);
            if d < best_d { best_d = d; best_c = c; }
        }

        let mut heap = KnnHeap::new();
        self.probe_cluster(best_c, q, &mut heap);
        QueryTrace { fraud: heap.fraud_count(), final_worst: heap.gate() }
    }

    /// Rank top-nprobe centroids (max-heap selection), probe them in sorted order with bbox pruning.
    pub fn query_topn(&self, q: *const i16, nprobe: usize) -> QueryTrace {
        let k = self.k;
        let nprobe = nprobe.min(64).min(k);

        // Max-heap of (dist, cluster_id) for top-nprobe selection
        let mut bc = [0u32; 64];
        let mut bd = [0u64; 64];
        let mut used = 0usize;
        let mut worst = 0u64;
        let mut wi = 0usize;

        macro_rules! push_candidate {
            ($c:expr, $d:expr) => {
                let d = $d; let c = $c;
                if used < nprobe {
                    bc[used] = c as u32; bd[used] = d;
                    if used == 0 || d > worst { worst = d; wi = used; }
                    used += 1;
                } else if d < worst {
                    bc[wi] = c as u32; bd[wi] = d;
                    worst = bd[0]; wi = 0;
                    for i in 1..used { if bd[i] > worst { worst = bd[i]; wi = i; } }
                }
            }
        }

        #[cfg(target_arch = "x86_64")]
        unsafe {
            let mut c = 0usize;
            while c + 8 <= k {
                let dists = self.avx2_dist8(q, c);
                for lane in 0..8 { push_candidate!(c + lane, dists[lane] as u64); }
                c += 8;
            }
            while c < k {
                push_candidate!(c, self.centroid_dist_scalar(q, c));
                c += 1;
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        for c in 0..k {
            push_candidate!(c, self.centroid_dist_scalar(q, c));
        }

        // Insertion sort (nprobe ≤ 64)
        for i in 1..used {
            let tc = bc[i]; let td = bd[i]; let mut j = i;
            while j > 0 && bd[j - 1] > td { bd[j] = bd[j-1]; bc[j] = bc[j-1]; j -= 1; }
            bd[j] = td; bc[j] = tc;
        }

        let mut heap = KnnHeap::new();
        for i in 0..used {
            let c = bc[i] as usize;
            if i > 0 && self.bbox_lb(q, c) >= heap.gate() { continue; }
            // Prefetch next cluster
            if i + 1 < used {
                let nc = bc[i + 1] as usize;
                unsafe {
                    let nxt = *self.offsets.add(nc) as usize;
                    let vp = self.vecs.add(nxt * DIMS * BLOCK) as *const u8;
                    let lp = self.labels.add(nxt * BLOCK);
                    std::arch::x86_64::_mm_prefetch(vp as *const i8, std::arch::x86_64::_MM_HINT_T1);
                    std::arch::x86_64::_mm_prefetch(lp as *const i8, std::arch::x86_64::_MM_HINT_T1);
                }
            }
            self.probe_cluster(c, q, &mut heap);
        }

        QueryTrace { fraud: heap.fraud_count(), final_worst: heap.gate() }
    }
}

/// Fixed-size max-heap tracking 5 nearest neighbours.
pub struct KnnHeap {
    dist:     [u64; 5],
    label:    [u8; 5],
    gate_idx: usize,
}

impl KnnHeap {
    #[inline]
    pub fn new() -> Self {
        KnnHeap { dist: [u64::MAX; 5], label: [0; 5], gate_idx: 0 }
    }

    #[inline]
    pub fn gate(&self) -> u64 { self.dist[self.gate_idx] }

    #[inline]
    pub fn insert(&mut self, d: u64, l: u8) {
        if d >= self.gate() { return; }
        self.dist[self.gate_idx]  = d;
        self.label[self.gate_idx] = l;
        self.gate_idx = 0;
        for i in 1..5 { if self.dist[i] > self.dist[self.gate_idx] { self.gate_idx = i; } }
    }

    #[inline]
    pub fn fraud_count(&self) -> u8 {
        self.label.iter().sum()
    }
}

/// Quantise a [0,1] float to i16 in [0, 10000].
#[inline]
pub fn qclamp01(v: f64) -> i16 {
    let v = v.clamp(0.0, 1.0);
    (v * 10000.0).round() as i16
}

/// Quantise a [-1,1] float to i16 in [-10000, 10000].
#[inline]
pub fn qround(v: f64) -> i16 {
    let v = v.clamp(-1.0, 1.0);
    (v * 10000.0).round() as i16
}
