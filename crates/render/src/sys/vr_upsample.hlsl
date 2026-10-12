// Replacement for CSVRUpsampleBilateral (the game's: per 2x2 block, bilinear in
// flat tiles, 1/|dz| weights in edge tiles, nearest otherwise). This one is a
// joint bilateral upsample per full-resolution pixel: bilinear over the four
// half-resolution taps weighted by relative depth similarity, widened to the
// neighbouring froxel tiles when none of the four matches, nearest depth as a
// last resort. The fog edge then follows the full-resolution (jittered) depth,
// which temporal AA and DLSS can resolve.
// TILE_SHIFT is defined before this text: 2, 3 or 4 (the game's variants).

cbuffer CBScreen : register(b1)
{
    uint2 iViewSize : packoffset(c2.z);
};

cbuffer CBVRCommon : register(b3)
{
    uint2 froxelDim : packoffset(c1.x);
    uint2 checkerDim : packoffset(c1.z);
    uint iFrameCount : packoffset(c101.x);
};

Texture2D<float> tDepthMap : register(t0);
Texture2D<float3> tLuminance : register(t1);
Texture2D<float3> tTransmittance : register(t2);
ByteAddressBuffer ditherMap : register(t3);
Texture2D<float> tCheckerDepth : register(t5);
RWTexture2D<float3> rwFinalL : register(u0);
RWTexture2D<float3> rwFinalT : register(u1);

// Relative depth difference at which a tap's weight halves (squared falloff).
static const float TOLERANCE = 0.04;

struct Acc
{
    float3 l;
    float3 t;
    float w;
    float best;
    float3 bl;
    float3 bt;
};

void tap(inout Acc acc, int2 q, float d, float spatial)
{
    q = clamp(q, int2(0, 0), int2(checkerDim) - 1);
    float di = tCheckerDepth.Load(int3(q, 0));
    float3 li = tLuminance.Load(int3(q, 0));
    float3 ti = tTransmittance.Load(int3(q, 0));
    float diff = abs(d - di);
    float rel = diff / max(max(abs(d), abs(di)), 1e-6);
    float x = rel / TOLERANCE;
    float w = spatial * exp2(-x * x);
    acc.l += li * w;
    acc.t += ti * w;
    acc.w += w;
    if (diff < acc.best)
    {
        acc.best = diff;
        acc.bl = li;
        acc.bt = ti;
    }
}

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint2 p = id.xy;
    if (any(p >= iViewSize))
        return;
    uint2 tile = p >> TILE_SHIFT;
    // Alpha-dithered tiles belong to the near-depth pass that follows.
    if (ditherMap.Load((tile.y * froxelDim.x + tile.x) * 4) == iFrameCount)
        return;

    float d = tDepthMap.Load(int3(p, 0));
    float2 c = (float2(p) + 0.5) * 0.5 - 0.5;
    int2 b = int2(floor(c));
    float2 f = c - float2(b);

    Acc acc;
    acc.l = 0;
    acc.t = 0;
    acc.w = 0;
    acc.best = 3.4e38;
    acc.bl = 0;
    acc.bt = 0;
    tap(acc, b, d, (1 - f.x) * (1 - f.y));
    tap(acc, b + int2(1, 0), d, f.x * (1 - f.y));
    tap(acc, b + int2(0, 1), d, (1 - f.x) * f.y);
    tap(acc, b + int2(1, 1), d, f.x * f.y);

    if (acc.w < 0.25)
    {
        int s = 1 << (TILE_SHIFT - 1); // half-resolution texels per tile
        [unroll] for (int y = -1; y <= 1; y++)
        {
            [unroll] for (int x = -1; x <= 1; x++)
            {
                if (x != 0 || y != 0)
                    tap(acc, b + int2(x, y) * s, d, 0.25);
            }
        }
    }

    float3 l = acc.bl;
    float3 t = acc.bt;
    if (acc.w > 1e-3)
    {
        l = acc.l / acc.w;
        t = acc.t / acc.w;
    }
    rwFinalL[p] = l;
    rwFinalT[p] = t;
}
