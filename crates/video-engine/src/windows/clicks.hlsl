// Click highlights blended onto the recorded frame: one quad per mark, alpha blended by the
// output merger, so the frame is never read back. Mirrors coverage() in clicks.rs tests.
struct Mark { float4 shape; float4 color; }; // shape: center x, y, radius, ring width (0 = dot)
cbuffer Clicks : register(b0) {
  float2 screen; uint count; uint unused;
  Mark marks[16];
};
struct Pixel {
  float4 position : SV_Position;
  float2 offset : OFFSET;
  nointerpolation uint mark : MARK;
};
Pixel vs(uint vertex : SV_VertexID, uint instance : SV_InstanceID) {
  Mark m = marks[instance];
  float reach = m.shape.z + m.shape.w * 0.5 + 1.5;
  float2 corner = float2((vertex & 1) ? 1.0 : -1.0, (vertex & 2) ? 1.0 : -1.0);
  float2 at = m.shape.xy + corner * reach;
  Pixel p;
  p.position = float4(at.x / screen.x * 2.0 - 1.0, 1.0 - at.y / screen.y * 2.0, 0.0, 1.0);
  p.offset = corner * reach;
  p.mark = instance;
  return p;
}
float4 ps(Pixel p) : SV_Target {
  Mark m = marks[p.mark];
  float distance = length(p.offset);
  // One pixel of antialiasing on each edge.
  float coverage = m.shape.w > 0.0
    ? saturate(m.shape.w * 0.5 + 0.5 - abs(distance - m.shape.z))
    : saturate(m.shape.z + 0.5 - distance);
  return float4(m.color.rgb, m.color.a * coverage);
}
