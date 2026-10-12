//! GPU colour statistics: the reduction shader, its output layout, and the
//! CPU side that finishes it.
//!
//! Output buffer (dwords): `[0, BINS)` the luma histogram, then one
//! [`PARTIAL_DWORDS`]-dword record per 16×16 group, row-major:
//! `sum.rgb (f32) · finite (u32) · min.rgb · max.rgb · nan_or_inf (u32) ·
//! above_one (u32)`. Histogram bin 0 holds luma ≤ 2^-16 (zero and negative
//! included), bins `1..=256` are eight per octave from 2^-16 to 2^16, bin
//! 257 holds luma ≥ 2^16. Non-finite pixels are only counted.

use super::ColourStats;

/// Group side (`[numthreads(16, 16, 1)]`).
pub const GROUP: u32 = 16;
/// Histogram bins per octave.
pub const BINS_PER_OCTAVE: u32 = 8;
/// log2 of the lowest binned luma.
pub const MIN_LOG2: i32 = -16;
/// log2 of the highest binned luma.
pub const MAX_LOG2: i32 = 16;
/// Histogram bins, including the underflow and overflow bins.
pub const BINS: usize = 2 + ((MAX_LOG2 - MIN_LOG2) as usize) * BINS_PER_OCTAVE as usize;
/// Dwords per group record.
pub const PARTIAL_DWORDS: usize = 12;
/// Threads per group of the histogram clear (`[numthreads(64, 1, 1)]`).
pub const CLEAR_GROUP: u32 = 64;

/// Rec. 709 luma of linear RGB.
pub fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The histogram bin of `luma` (mirrors `BinOf` in the shader).
pub fn bin_of(luma: f32) -> usize {
    if luma.is_nan() || luma <= (MIN_LOG2 as f32).exp2() {
        return 0;
    }
    if luma >= (MAX_LOG2 as f32).exp2() {
        return BINS - 1;
    }
    let position = (luma.log2() - MIN_LOG2 as f32) * BINS_PER_OCTAVE as f32;
    1 + (position.floor().max(0.0) as usize).min(BINS - 3)
}

/// Thread groups for a `width × height` source.
pub fn groups(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(GROUP), height.div_ceil(GROUP))
}

/// Output buffer size in dwords.
pub fn buffer_dwords(width: u32, height: u32) -> usize {
    let (x, y) = groups(width, height);
    BINS + x as usize * y as usize * PARTIAL_DWORDS
}

/// The `q` quantile (`0..=1`) of the luma histogram, interpolated
/// geometrically inside the bin; 0 for an empty histogram.
pub fn percentile(histogram: &[u32], q: f64) -> f32 {
    let total: u64 = histogram.iter().map(|&n| u64::from(n)).sum();
    if total == 0 {
        return 0.0;
    }
    let target = q.clamp(0.0, 1.0) * total as f64;
    let mut below = 0u64;
    for (bin, &count) in histogram.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let through = below + u64::from(count);
        if through as f64 >= target {
            let fraction = ((target - below as f64) / f64::from(count)).clamp(0.0, 1.0);
            return value_in_bin(bin, fraction);
        }
        below = through;
    }
    value_in_bin(BINS - 1, 1.0)
}

fn value_in_bin(bin: usize, fraction: f64) -> f32 {
    if bin == 0 {
        return 0.0;
    }
    if bin >= BINS - 1 {
        return (MAX_LOG2 as f32).exp2();
    }
    let octaves = MIN_LOG2 as f64 + (bin as f64 - 1.0 + fraction) / f64::from(BINS_PER_OCTAVE);
    octaves.exp2() as f32
}

/// Finish the reduction of a `width × height` source from the shader's
/// output; `None` if `data` is not the size the shader writes.
pub fn reduce(data: &[u32], width: u32, height: u32) -> Option<ColourStats> {
    if data.len() != buffer_dwords(width, height) {
        return None;
    }
    let (histogram, partials) = data.split_at(BINS);
    let f = |bits: u32| f32::from_bits(bits);
    let mut sum = [0f64; 3];
    let mut finite = 0u64;
    let mut nan_or_inf = 0u64;
    let mut above = 0u64;
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let (records, _) = partials.as_chunks::<PARTIAL_DWORDS>();
    for record in records {
        nan_or_inf += u64::from(record[10]);
        above += u64::from(record[11]);
        if record[3] == 0 {
            continue;
        }
        finite += u64::from(record[3]);
        for c in 0..3 {
            sum[c] += f64::from(f(record[c]));
            min[c] = min[c].min(f(record[4 + c]));
            max[c] = max[c].max(f(record[7 + c]));
        }
    }
    let pixels = u64::from(width) * u64::from(height);
    let (min, max, mean) = if finite == 0 {
        ([0.0; 3], [0.0; 3], [0.0; 3])
    } else {
        (min, max, sum.map(|s| (s / finite as f64) as f32))
    };
    Some(ColourStats {
        min,
        max,
        mean,
        p50: percentile(histogram, 0.5),
        p99: percentile(histogram, 0.99),
        p999: percentile(histogram, 0.999),
        above_one: if pixels == 0 {
            0.0
        } else {
            (above as f64 / pixels as f64) as f32
        },
        nan_or_inf,
    })
}

/// The format to read a colour resource of `format` through: typeless
/// colour families get their float/unorm member; depth, integer and
/// block-compressed formats have none (`None`).
pub fn view_format(format: u32) -> Option<u32> {
    match format {
        // R32G32B32A32, R16G16B16A16, R10G10B10A2, R8G8B8A8, B8G8R8A8,
        // B8G8R8X8 typeless.
        1 => Some(2),
        9 => Some(10),
        23 => Some(24),
        27 => Some(28),
        90 => Some(87),
        92 => Some(88),
        // Readable float / unorm colour formats as they are.
        2 | 6 | 10 | 11 | 16 | 24 | 26 | 28 | 29 | 34 | 35 | 41 | 49 | 54 | 56 | 61 | 87 | 88
        | 91 | 93 => Some(format),
        _ => None,
    }
}

/// Root constants: `Width, Height, GroupsX, Pad`.
pub fn constants(width: u32, height: u32) -> [u32; 4] {
    [width, height, groups(width, height).0, 0]
}

/// The shader (`cs_5_0`): entry `clear` zeroes the histogram
/// (`ceil(BINS / 64)` groups), entry `reduce` runs over the source. `t0` the
/// source, `u0` the output (raw buffer), `b0` [`constants`].
pub const HLSL: &str = r#"
Texture2D<float4>   Source : register(t0);
RWByteAddressBuffer Out    : register(u0);

cbuffer Params : register(b0)
{
    uint Width;
    uint Height;
    uint GroupsX;
    uint Pad;
};

#define BINS 258
#define RECORD 12

groupshared uint   gsHist[BINS];
groupshared float3 gsSum[256];
groupshared float3 gsMin[256];
groupshared float3 gsMax[256];
groupshared uint3  gsCount[256];

uint BinOf(float l)
{
    if (!(l > exp2(-16.0))) { return 0; }
    if (l >= 65536.0) { return BINS - 1; }
    return 1 + min(BINS - 3, (uint)max(floor((log2(l) + 16.0) * 8.0), 0.0));
}

[numthreads(64, 1, 1)]
void clear(uint3 id : SV_DispatchThreadID)
{
    if (id.x < BINS) { Out.Store(id.x * 4, 0); }
}

[numthreads(16, 16, 1)]
void reduce(uint3 id : SV_DispatchThreadID, uint3 group : SV_GroupID, uint index : SV_GroupIndex)
{
    for (uint b = index; b < BINS; b += 256) { gsHist[b] = 0; }
    GroupMemoryBarrierWithGroupSync();

    float3 sum = float3(0.0, 0.0, 0.0);
    float3 lo = float3(3.0e38, 3.0e38, 3.0e38);
    float3 hi = float3(-3.0e38, -3.0e38, -3.0e38);
    uint3 count = uint3(0, 0, 0); // finite, nan_or_inf, above_one
    if (id.x < Width && id.y < Height)
    {
        float3 c = Source.Load(int3(id.xy, 0)).rgb;
        if (isfinite(c.r) && isfinite(c.g) && isfinite(c.b))
        {
            sum = c; lo = c; hi = c;
            count.x = 1;
            count.z = max(c.r, max(c.g, c.b)) > 1.0 ? 1 : 0;
            uint unused;
            InterlockedAdd(gsHist[BinOf(dot(c, float3(0.2126, 0.7152, 0.0722)))], 1, unused);
        }
        else
        {
            count.y = 1;
        }
    }
    gsSum[index] = sum;
    gsMin[index] = lo;
    gsMax[index] = hi;
    gsCount[index] = count;
    GroupMemoryBarrierWithGroupSync();

    for (uint step = 128; step > 0; step >>= 1)
    {
        if (index < step)
        {
            gsSum[index] += gsSum[index + step];
            gsMin[index] = min(gsMin[index], gsMin[index + step]);
            gsMax[index] = max(gsMax[index], gsMax[index + step]);
            gsCount[index] += gsCount[index + step];
        }
        GroupMemoryBarrierWithGroupSync();
    }

    for (uint bin = index; bin < BINS; bin += 256)
    {
        uint n = gsHist[bin];
        if (n != 0) { uint previous; Out.InterlockedAdd(bin * 4, n, previous); }
    }
    if (index == 0)
    {
        uint at = (BINS + (group.y * GroupsX + group.x) * RECORD) * 4;
        Out.Store3(at, asuint(gsSum[0]));
        Out.Store(at + 12, gsCount[0].x);
        Out.Store3(at + 16, asuint(gsMin[0]));
        Out.Store3(at + 28, asuint(gsMax[0]));
        Out.Store2(at + 40, gsCount[0].yz);
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_constants_agree_with_the_shader() {
        assert_eq!(BINS, 258);
        assert!(HLSL.contains("#define BINS 258"));
        assert!(HLSL.contains(&format!("#define RECORD {PARTIAL_DWORDS}")));
        assert_eq!(groups(3840, 2160), (240, 135));
        assert_eq!(groups(17, 1), (2, 1));
        assert_eq!(buffer_dwords(16, 16), BINS + PARTIAL_DWORDS);
        assert_eq!(constants(33, 20), [33, 20, 3, 0]);
    }

    #[test]
    fn bins_are_eight_per_octave() {
        assert_eq!(bin_of(0.0), 0);
        assert_eq!(bin_of(-5.0), 0);
        assert_eq!(bin_of(f32::NAN), 0);
        assert_eq!(bin_of((-16f32).exp2()), 0, "the lower edge is underflow");
        assert_eq!(bin_of(1.0), 1 + 16 * 8);
        assert_eq!(
            bin_of(1.5),
            1 + 16 * 8 + 4,
            "log2(1.5) = 0.585 -> 4th eighth"
        );
        assert_eq!(bin_of(0.5), 1 + 15 * 8);
        assert_eq!(bin_of(65535.0), BINS - 2);
        assert_eq!(bin_of(65536.0), BINS - 1);
        assert_eq!(bin_of(f32::INFINITY), BINS - 1);
    }

    fn histogram_of(lumas: &[f32]) -> Vec<u32> {
        let mut histogram = vec![0u32; BINS];
        for &l in lumas {
            histogram[bin_of(l)] += 1;
        }
        histogram
    }

    #[test]
    fn percentiles_land_in_the_right_bin() {
        assert_eq!(percentile(&vec![0; BINS], 0.5), 0.0);
        // 1000 pixels at 0.18, 10 at 4.0: median ~0.18, p99.9 ~4.
        let mut lumas = vec![0.18f32; 1000];
        lumas.extend([4.0f32; 10]);
        let histogram = histogram_of(&lumas);
        let p50 = percentile(&histogram, 0.5);
        let width = 2f32.powf(1.0 / 8.0);
        assert!(p50 >= 0.18 / width && p50 <= 0.18 * width, "{p50}");
        let p999 = percentile(&histogram, 0.999);
        assert!(p999 >= 4.0 / width && p999 <= 4.0 * width, "{p999}");
        // Monotonic in q.
        let qs = [0.0, 0.1, 0.5, 0.9, 0.99, 0.999, 1.0];
        let values: Vec<f32> = qs.iter().map(|&q| percentile(&histogram, q)).collect();
        assert!(values.windows(2).all(|w| w[0] <= w[1]), "{values:?}");
        // Under- and overflow bins.
        assert_eq!(percentile(&histogram_of(&[0.0; 4]), 0.5), 0.0);
        assert_eq!(percentile(&histogram_of(&[1e9; 4]), 0.5), 65536.0);
    }

    fn record(
        sum: [f32; 3],
        finite: u32,
        min: [f32; 3],
        max: [f32; 3],
        nan: u32,
        above: u32,
    ) -> Vec<u32> {
        let mut r = Vec::with_capacity(PARTIAL_DWORDS);
        r.extend(sum.map(f32::to_bits));
        r.push(finite);
        r.extend(min.map(f32::to_bits));
        r.extend(max.map(f32::to_bits));
        r.push(nan);
        r.push(above);
        r
    }

    #[test]
    fn reduction_combines_group_records() {
        // 32 × 16: two groups. The second has only non-finite pixels, whose
        // sentinel minima and maxima must not leak.
        let mut data = histogram_of(&[0.5; 256]);
        data.extend(record(
            [128.0, 256.0, 512.0],
            256,
            [0.1, 0.2, 0.3],
            [2.0, 1.0, 4.0],
            0,
            3,
        ));
        data.extend(record([0.0; 3], 0, [3.0e38; 3], [-3.0e38; 3], 256, 0));
        let stats = reduce(&data, 32, 16).unwrap();
        assert_eq!(stats.mean, [0.5, 1.0, 2.0]);
        assert_eq!(stats.min, [0.1, 0.2, 0.3]);
        assert_eq!(stats.max, [2.0, 1.0, 4.0]);
        assert_eq!(stats.nan_or_inf, 256);
        assert_eq!(stats.above_one, 3.0 / 512.0);
        assert!((stats.p50 - 0.5).abs() < 0.05, "{}", stats.p50);
        assert_eq!(reduce(&data[1..], 32, 16), None, "wrong size");
        // All non-finite: zeros, not sentinels.
        let mut empty = vec![0u32; BINS];
        empty.extend(record([0.0; 3], 0, [3.0e38; 3], [-3.0e38; 3], 256, 0));
        let stats = reduce(&empty, 16, 16).unwrap();
        assert_eq!(
            (stats.min, stats.max, stats.mean),
            ([0.0; 3], [0.0; 3], [0.0; 3])
        );
    }

    #[test]
    fn colour_is_read_through_a_typed_view() {
        assert_eq!(view_format(26), Some(26), "R11G11B10_FLOAT");
        assert_eq!(view_format(9), Some(10), "R16G16B16A16_TYPELESS");
        assert_eq!(view_format(27), Some(28));
        assert_eq!(view_format(90), Some(87));
        for format in [0, 40, 45, 20, 42, 71] {
            assert_eq!(view_format(format), None, "{format}");
        }
    }
}
