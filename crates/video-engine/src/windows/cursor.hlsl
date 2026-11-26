Texture2D<float4> desktop : register(t0);
Texture2D<uint> shape : register(t1);
RWTexture2D<float4> result : register(u0);
cbuffer Cursor : register(b0) {
  int cursor_x; int cursor_y; uint shape_width; uint shape_height;
  uint shape_kind; uint shape_pitch; uint screen_width; uint screen_height;
};
uint byte_at(uint x, uint y) { return shape.Load(int3(x,y,0)); }
[numthreads(8,8,1)]
void main(uint3 id : SV_DispatchThreadID) {
  if (id.x >= shape_width || id.y >= shape_height) return;
  int2 location = int2(cursor_x,cursor_y) + int2(id.xy);
  if (location.x < 0 || location.y < 0 || location.x >= int(screen_width) || location.y >= int(screen_height)) return;
  float4 background = desktop.Load(int3(location,0));
  uint3 base = uint3(round(saturate(background.rgb)*255.0));
  float3 color;
  if (shape_kind == 1) {
    uint shift = 7-(id.x&7);
    uint a = (byte_at(id.x/8,id.y)>>shift)&1;
    uint x = (byte_at(id.x/8,id.y+shape_height)>>shift)&1;
    color = float3((base & (a*255)) ^ (x*255))/255.0;
  } else {
    uint offset = id.x*4;
    uint3 rgb = uint3(byte_at(offset+2,id.y),byte_at(offset+1,id.y),byte_at(offset,id.y));
    uint alpha = byte_at(offset+3,id.y);
    if (shape_kind == 4) color = alpha == 0 ? float3(rgb)/255.0 : float3(base ^ rgb)/255.0;
    else color = float3(rgb)/255.0 + background.rgb * (1.0-float(alpha)/255.0);
  }
  result[location] = float4(saturate(color),1.0);
}
