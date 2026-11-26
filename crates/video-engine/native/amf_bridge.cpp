// Narrow C ABI for the unmodified AMD public C++ interfaces. MIT, Flinch authors.
#include <windows.h>
#include <new>
#include <atomic>
#include <cstdint>
#include <cstring>
#include "amf/core/Factory.h"
#include "amf/components/VideoEncoderVCE.h"

struct SlotObserver : amf::AMFSurfaceObserver {
    std::atomic<bool> busy{false};
    void AMF_STD_CALL OnSurfaceDataRelease(amf::AMFSurface*) override { busy.store(false, std::memory_order_release); }
};
// Pool slots; must equal the engine's limits::MAX_SURFACES (checked by a Rust test).
constexpr unsigned kSlots = 16;
struct FlinchAmf {
    HMODULE module{};
    amf::AMFContextPtr context;
    amf::AMFComponentPtr encoder;
    amf::AMFDataPtr packet;
    SlotObserver slots[kSlots];
    ~FlinchAmf() {
        packet = nullptr;
        if (encoder) { encoder->Terminate(); encoder = nullptr; }
        if (context) { context->Terminate(); context = nullptr; }
        if (module) FreeLibrary(module);
    }
};
extern "C" {
int flinch_amf_version(uint64_t* version) {
    HMODULE module = LoadLibraryExW(L"amfrt64.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!module) return -static_cast<int>(GetLastError());
    auto query = reinterpret_cast<AMFQueryVersion_Fn>(GetProcAddress(module, AMF_QUERY_VERSION_FUNCTION_NAME));
    int result = query ? query(version) : -static_cast<int>(ERROR_PROC_NOT_FOUND);
    FreeLibrary(module); return result;
}
int flinch_amf_open(void* device, FlinchAmf** output) {
    auto value = new(std::nothrow) FlinchAmf;
    if (!value) return AMF_OUT_OF_MEMORY;
    value->module = LoadLibraryExW(L"amfrt64.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!value->module) { int e = -static_cast<int>(GetLastError()); delete value; return e; }
    auto init = reinterpret_cast<AMFInit_Fn>(GetProcAddress(value->module, AMF_INIT_FUNCTION_NAME));
    if (!init) { delete value; return -static_cast<int>(ERROR_PROC_NOT_FOUND); }
    amf::AMFFactory* factory{};
    AMF_RESULT result = init(AMF_FULL_VERSION, &factory);
    if (result == AMF_OK) result = factory->CreateContext(&value->context);
    if (result == AMF_OK) result = value->context->InitDX11(device);
    if (result == AMF_OK) result = factory->CreateComponent(value->context, AMFVideoEncoderVCE_AVC, &value->encoder);
    if (result != AMF_OK) { delete value; return result; }
    *output = value; return AMF_OK;
}
int flinch_amf_int(FlinchAmf* value, const wchar_t* key, int64_t wanted, int boolean) {
    AMF_RESULT result = boolean ? value->encoder->SetProperty(key, wanted != 0) : value->encoder->SetProperty(key, static_cast<amf_int64>(wanted));
    if (result != AMF_OK) return result;
    amf::AMFVariant actual;
    result = value->encoder->GetProperty(key, &actual);
    if (result != AMF_OK) return result;
    if (boolean) return actual.ToBool() == (wanted != 0) ? AMF_OK : AMF_INVALID_ARG;
    return actual.ToInt64() == wanted ? AMF_OK : AMF_INVALID_ARG;
}
int flinch_amf_query_wait(FlinchAmf* value, int* supported) {
    amf::AMFCapsPtr caps;
    AMF_RESULT result = value->encoder->GetCaps(&caps);
    if (result != AMF_OK) return result;
    amf_bool available = false;
    result = caps->GetProperty(AMF_VIDEO_ENCODER_CAP_QUERY_TIMEOUT_SUPPORT, &available);
    if (result == AMF_NOT_FOUND) { *supported = 0; return AMF_OK; }
    if (result != AMF_OK) return result;
    *supported = available ? 1 : 0;
    if (!available) return AMF_OK;
    result = value->encoder->SetProperty(AMF_VIDEO_ENCODER_QUERY_TIMEOUT, static_cast<amf_int64>(2));
    if (result != AMF_OK) return result;
    amf_int64 actual = 0;
    result = value->encoder->GetProperty(AMF_VIDEO_ENCODER_QUERY_TIMEOUT, &actual);
    return result == AMF_OK && actual != 2 ? AMF_INVALID_ARG : result;
}
int flinch_amf_init(FlinchAmf* value, int width, int height, int fps) {
    AMF_RESULT result = value->encoder->SetProperty(AMF_VIDEO_ENCODER_FRAMESIZE, AMFConstructSize(width, height));
    if (result == AMF_OK) result = value->encoder->SetProperty(AMF_VIDEO_ENCODER_FRAMERATE, AMFConstructRate(fps, 1));
    if (result == AMF_OK) result = value->encoder->Init(amf::AMF_SURFACE_NV12, width, height);
    return result;
}
int flinch_amf_extra(FlinchAmf* value, uint8_t* output, size_t capacity, size_t* size) {
    amf::AMFVariant extra;
    AMF_RESULT result = value->encoder->GetProperty(AMF_VIDEO_ENCODER_EXTRADATA, &extra);
    if (result != AMF_OK) return result;
    amf::AMFBufferPtr buffer(extra.pInterface);
    if (!buffer) return AMF_INVALID_FORMAT;
    *size = buffer->GetSize();
    if (capacity < *size) return AMF_INPUT_FULL;
    memcpy(output, buffer->GetNative(), *size); return AMF_OK;
}
int flinch_amf_busy(FlinchAmf* value, unsigned slot) {
    if (slot >= kSlots) return 1;
    return value->slots[slot].busy.load(std::memory_order_acquire) ? 1 : 0;
}
int flinch_amf_submit(FlinchAmf* value, void* texture, unsigned slot, int64_t pts, int64_t duration, int keyframe) {
    if (slot >= kSlots || value->slots[slot].busy.exchange(true)) return AMF_INPUT_FULL;
    amf::AMFSurfacePtr surface;
    AMF_RESULT result = value->context->CreateSurfaceFromDX11Native(texture, &surface, &value->slots[slot]);
    if (result != AMF_OK) { value->slots[slot].busy.store(false); return result; }
    surface->SetPts(pts); surface->SetDuration(duration);
    if (keyframe) {
        result = surface->SetProperty(AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE, static_cast<amf_int64>(AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR));
        if (result != AMF_OK) return result;
    }
    return value->encoder->SubmitInput(surface);
}
int flinch_amf_poll(FlinchAmf* value, const uint8_t** data, size_t* size, int64_t* pts, int64_t* duration, int* keyframe) {
    value->packet = nullptr;
    AMF_RESULT result = value->encoder->QueryOutput(&value->packet);
    if (result != AMF_OK) return result;
    if (!value->packet) return AMF_REPEAT;
    amf::AMFBufferPtr buffer(value->packet);
    if (!buffer) return AMF_INVALID_FORMAT;
    amf_int64 type = -1;
    result = buffer->GetProperty(AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE, &type);
    if (result != AMF_OK) return result;
    *data = static_cast<const uint8_t*>(buffer->GetNative()); *size = buffer->GetSize();
    *pts = buffer->GetPts(); *duration = buffer->GetDuration();
    *keyframe = type == AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR || type == AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_I;
    return AMF_OK;
}
int flinch_amf_drain(FlinchAmf* value) { return value->encoder->Drain(); }
void flinch_amf_close(FlinchAmf* value) { delete value; }
}
