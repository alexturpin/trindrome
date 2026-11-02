// Trindrome GPU search kernels.
//
// The GPU only *rejects*: it builds each binary/hexadecimal palindrome candidate
// and applies the exact decimal window filter from docs/cuda-design.md. Survivors
// are reported by offset for exact verification on the CPU. Every step must match
// trindrome_core::filter bit for bit, checksum included.

#include <stdint.h>

#define MAX_LIMBS 16

// Must match `Params` in src/lib.rs (repr(C)).
struct Params {
    uint32_t hex_digits;
    uint32_t leading;
    uint32_t free_digits;
    uint32_t radix_bits;
    uint32_t alphabet[4];
    uint32_t window;
    uint32_t ten_window;
    uint32_t digits_low;
    uint32_t has_threshold;
    uint32_t residues[MAX_LIMBS];   // 2^(32 i) mod 10^9
    uint32_t threshold[MAX_LIMBS];  // 10^digits_low, if has_threshold
    uint32_t p_low[MAX_LIMBS];      // 10^(digits_low - window)
    uint32_t p_high[MAX_LIMBS];     // 10^(digits_low + 1 - window)
};

// counters[0] survivors (keeps counting past capacity), [1] processed, [2] checksum.
#define SURVIVORS 0
#define PROCESSED 1
#define CHECKSUM 2

__device__ __forceinline__ uint64_t mix64(uint64_t z) {
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ull;
    z = (z ^ (z >> 27)) * 0x94D049BB133111EBull;
    return z ^ (z >> 31);
}

// Builds the candidate at `index` into `v` (little-endian 32-bit limbs).
template <int N>
__device__ __forceinline__ void construct(const Params& p, uint64_t index, uint32_t (&v)[N]) {
    // Nibble positions depend on the runtime length, so assemble in an
    // addressable scratch array and copy into registers afterwards.
    uint32_t scratch[N];
#pragma unroll
    for (int i = 0; i < N; ++i) scratch[i] = 0;

    const uint32_t last = p.hex_digits - 1;
    scratch[0] |= p.leading;
    scratch[last >> 3] |= p.leading << (4 * (last & 7));

    const uint32_t mask = (1u << p.radix_bits) - 1;
    for (uint32_t pos = 1; pos <= p.free_digits; ++pos) {
        // Prefix position `pos` holds index digit (k - pos), most significant first.
        const uint32_t shift = (p.free_digits - pos) * p.radix_bits;
        const uint32_t sel = shift < 64 ? (uint32_t)(index >> shift) & mask : 0;
        const uint32_t digit = p.alphabet[sel];
        const uint32_t lo = pos, hi = last - pos;
        scratch[lo >> 3] |= digit << (4 * (lo & 7));
        scratch[hi >> 3] |= digit << (4 * (hi & 7));
    }

#pragma unroll
    for (int i = 0; i < N; ++i) v[i] = scratch[i];
}

struct Evaluation {
    uint32_t digits, low, pass, borrow, diff_low32, limb_xor;
};

template <int N>
__device__ __forceinline__ Evaluation evaluate(const Params& p, const uint32_t (&v)[N]) {
    Evaluation e;

    // Digit count: at most one power of ten lies inside a search space.
    uint32_t above = 0;
    if (p.has_threshold) {
        above = 1;  // equal counts as above
#pragma unroll
        for (int i = N - 1; i >= 0; --i) {
            if (v[i] != p.threshold[i]) {
                above = v[i] > p.threshold[i];
                break;
            }
        }
    }
    e.digits = p.digits_low + above;

    // v mod 10^9 from limb residues, reducing every three terms so the sum fits.
    uint64_t acc = 0;
#pragma unroll
    for (int i = 0; i < N; ++i) {
        acc += (uint64_t)v[i] * p.residues[i];
        if (i % 3 == 2) acc %= 1000000000ull;
    }
    acc %= 1000000000ull;
    e.low = (uint32_t)acc % p.ten_window;

    uint32_t rho = 0, x = e.low;
#pragma unroll
    for (int i = 0; i < 9; ++i) {
        if (i < (int)p.window) {
            rho = rho * 10 + x % 10;
            x /= 10;
        }
    }

    // rho * P in N + 1 limbs, then diff = v - rho * P with borrow.
    uint32_t P[N + 1], rp[N + 1], diff[N + 1];
    uint64_t carry = 0;
#pragma unroll
    for (int i = 0; i < N; ++i) {
        P[i] = above ? p.p_high[i] : p.p_low[i];
        const uint64_t t = (uint64_t)P[i] * rho + carry;
        rp[i] = (uint32_t)t;
        carry = t >> 32;
    }
    P[N] = 0;
    rp[N] = (uint32_t)carry;

    uint32_t borrow = 0;
#pragma unroll
    for (int i = 0; i <= N; ++i) {
        const uint64_t vi = i < N ? v[i] : 0;
        const uint64_t t = vi - rp[i] - borrow;
        diff[i] = (uint32_t)t;
        borrow = (uint32_t)(t >> 63);
    }
    e.borrow = borrow;

    uint32_t below_p = 0;  // diff < P
#pragma unroll
    for (int i = N; i >= 0; --i) {
        if (diff[i] != P[i]) {
            below_p = diff[i] < P[i];
            break;
        }
    }
    e.pass = !borrow && below_p;
    e.diff_low32 = v[0] - rp[0];

    uint32_t fold = 0;
#pragma unroll
    for (int i = 0; i < N; ++i) fold ^= v[i];
    e.limb_xor = fold;
    return e;
}

__device__ __forceinline__ uint64_t hash(const Evaluation& e, uint64_t index) {
    const uint64_t a = (uint64_t)e.low | (uint64_t)e.digits << 32 | (uint64_t)e.pass << 40 |
                       (uint64_t)e.borrow << 41;
    const uint64_t b = (uint64_t)e.diff_low32 | (uint64_t)e.limb_xor << 32;
    return mix64(a ^ mix64(b ^ mix64(index)));
}

template <int N>
__device__ void search(const Params p, uint64_t start, uint64_t count, uint32_t* survivors,
                       uint32_t capacity, unsigned long long* counters) {
    uint64_t processed = 0, checksum = 0;
    const uint64_t stride = (uint64_t)gridDim.x * blockDim.x;
    for (uint64_t i = (uint64_t)blockIdx.x * blockDim.x + threadIdx.x; i < count; i += stride) {
        const uint64_t index = start + i;
        uint32_t v[N];
        construct<N>(p, index, v);
        const Evaluation e = evaluate<N>(p, v);
        checksum += hash(e, index);
        ++processed;
        if (e.pass) {
            const unsigned long long slot = atomicAdd(&counters[SURVIVORS], 1ull);
            if (slot < capacity) survivors[slot] = (uint32_t)i;
        }
    }
    atomicAdd(&counters[PROCESSED], (unsigned long long)processed);
    atomicAdd(&counters[CHECKSUM], (unsigned long long)checksum);
}

template <int N>
__device__ void construct_only(const Params p, uint64_t start, uint32_t count, uint32_t* out) {
    const uint32_t i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= count) return;
    uint32_t v[N];
    construct<N>(p, start + i, v);
#pragma unroll
    for (int j = 0; j < N; ++j) out[(uint64_t)i * N + j] = v[j];
}

#define INSTANTIATE(N)                                                                         \
    extern "C" __global__ void search_##N(const Params p, uint64_t start, uint64_t count,      \
                                          uint32_t* survivors, uint32_t capacity,              \
                                          unsigned long long* counters) {                      \
        search<N>(p, start, count, survivors, capacity, counters);                             \
    }                                                                                          \
    extern "C" __global__ void construct_##N(const Params p, uint64_t start, uint32_t count,   \
                                             uint32_t* out) {                                  \
        construct_only<N>(p, start, count, out);                                               \
    }

INSTANTIATE(4)
INSTANTIATE(8)
INSTANTIATE(12)
INSTANTIATE(16)
