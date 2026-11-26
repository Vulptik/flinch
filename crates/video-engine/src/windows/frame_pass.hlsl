Texture2D<float4> panel : register(t0);
RWTexture2D<float4> result : register(u0);
cbuffer FramePass : register(b0) {
  uint width; uint height; uint rotation; uint hdr;
  float white_scale; float3 unused;
};
// sRGB encoding: the inverse of the decode Windows applies to SDR content on an HDR desktop.
float3 srgb(float3 l) {
  return l <= 0.0031308 ? l * 12.92 : 1.055 * pow(l, 1.0 / 2.4) - 0.055;
}
// Desktop pixel id.xy of a width x height desktop shows this panel texel; mirrors
// Orientation::panel_texel in rotate.rs. Rotation codes are DXGI_MODE_ROTATION values.
// HDR texels are scRGB (linear BT.709, 1.0 = 80 nits); mirrors frame_pass::sdr_from_scrgb.
[numthreads(8,8,1)]
void main(uint3 id : SV_DispatchThreadID) {
  if (id.x >= width || id.y >= height) return;
  uint2 source;
  if (rotation == 2) source = uint2(id.y, width - 1 - id.x);
  else if (rotation == 3) source = uint2(width - 1 - id.x, height - 1 - id.y);
  else if (rotation == 4) source = uint2(height - 1 - id.y, id.x);
  else source = id.xy;
  float4 texel = panel.Load(int3(source, 0));
  if (hdr != 0) {
    float3 l = max(texel.rgb * white_scale, 0.0);
    float peak = max(l.r, max(l.g, l.b));
    if (peak > 1.0) l /= peak;
    texel = float4(srgb(l), 1.0);
  }
  result[id.xy] = texel;
}
