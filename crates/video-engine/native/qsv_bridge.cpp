// Narrow C ABI over Intel Quick Sync Video through the unmodified oneVPL dispatcher. MIT, Flinch authors.
//
// The calls follow the oneVPL 2.17 headers and the
// long-standing D3D11 external-allocator encode flow (Media SDK 1.x and oneVPL 2.x runtimes), but
// they have not run against a real Intel driver yet. Points that need hardware confirmation are
// marked HARDWARE-CHECK.
#include <windows.h>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <new>
#include <vector>
#include "vpl/mfx.h"

namespace {
// Pool slots; must equal the engine's limits::MAX_SURFACES (checked by a Rust test).
constexpr uint32_t kSlots = 16;
// Output buffers (each at least a raw frame of system memory): AsyncDepth bounds the encodes in
// flight well below this, so they do not grow with the surface pool.
constexpr uint32_t kBitstreams = 8;
constexpr uint32_t kTimings = kSlots * 2;
// Bridge results outside mfxStatus (whose values stay within -100..100).
constexpr int kRepeat = 1000;
constexpr int kEof = 1001;
constexpr int kInputFull = 1002;
constexpr int kUnsupported = 1003;
constexpr int kInvalid = 1004;
enum Stage {
    kStageLoad = 1, kStageFilter, kStageAdapter, kStageSession, kStageHandle, kStageAllocator,
    kStageQuery, kStageInitialize, kStageParameters, kStageBitstream
};
}  // namespace

struct FlinchQsvConfig {
    uint32_t width, height, fps;
    uint32_t constant_qp;  // 1: constant QP (qp_i/qp_p), 0: VBR at bitrate (bits per second)
    uint32_t qp_i, qp_p, bitrate, gop;
};

struct FlinchQsv {
    mfxLoader loader{};
    mfxSession session{};
    bool initialized{};
    mfxFrameAllocator allocator{};
    mfxHDLPair handles[kSlots]{};
    mfxFrameSurface1 surfaces[kSlots]{};
    mfxBitstream bitstreams[kBitstreams]{};
    std::vector<mfxU8> buffers[kBitstreams];
    bool bitstream_busy[kBitstreams]{};
    struct Pending { mfxSyncPoint sync; uint32_t bitstream; } queue[kBitstreams]{};
    uint32_t head{}, count{};
    // Submit order equals output order without B-frames; the runtime's 90 kHz stamps are not used.
    struct Timing { int64_t pts, duration; } timings[kTimings]{};
    uint32_t timing_head{}, timing_count{};
    int read{-1};  // bitstream the caller is reading, released on the next poll
    bool eos{};
    mfxU8 extra[1024]{};
    uint32_t extra_size{};
    char description[160]{};
    ~FlinchQsv() {
        if (initialized) MFXVideoENCODE_Close(session);
        if (session) MFXClose(session);
        if (loader) MFXUnload(loader);
    }
};

// Input surfaces are the engine's own NV12 textures, so the allocator only resolves handles.
static mfxStatus MFX_CDECL alloc_frames(mfxHDL, mfxFrameAllocRequest*, mfxFrameAllocResponse*) {
    return MFX_ERR_UNSUPPORTED;
}
static mfxStatus MFX_CDECL lock_frame(mfxHDL, mfxMemId, mfxFrameData*) { return MFX_ERR_UNSUPPORTED; }
static mfxStatus MFX_CDECL unlock_frame(mfxHDL, mfxMemId, mfxFrameData*) { return MFX_ERR_UNSUPPORTED; }
static mfxStatus MFX_CDECL frame_handle(mfxHDL, mfxMemId mid, mfxHDL* handle) {
    if (!mid || !handle) return MFX_ERR_INVALID_HANDLE;
    *handle = mid;  // D3D11: a pointer to {texture, subresource index}
    return MFX_ERR_NONE;
}
static mfxStatus MFX_CDECL free_frames(mfxHDL, mfxFrameAllocResponse*) { return MFX_ERR_NONE; }

static void note(char* text, size_t capacity, const char* message) {
    if (text && capacity) strncpy_s(text, capacity, message, _TRUNCATE);
}

static mfxStatus filter(mfxLoader loader, const char* name, mfxU32 wanted) {
    mfxConfig config = MFXCreateConfig(loader);
    if (!config) return MFX_ERR_NULL_PTR;
    mfxVariant value{};
    value.Type = MFX_VARIANT_TYPE_U32;
    value.Data.U32 = wanted;
    return MFXSetConfigFilterProperty(config, reinterpret_cast<const mfxU8*>(name), value);
}

// The implementation on the display's adapter, matched by LUID. Runtimes without extended device
// IDs (older Media SDK) are accepted only when they are the sole candidate.
static int choose(FlinchQsv* value, int64_t luid, mfxU32* index, char* text, size_t capacity) {
    mfxU32 unverified = 0, unverified_index = 0;
    for (mfxU32 i = 0;; ++i) {
        mfxHDL description = nullptr;
        mfxStatus status = MFXEnumImplementations(value->loader, i, MFX_IMPLCAPS_IMPLDESCSTRUCTURE, &description);
        if (status == MFX_ERR_NOT_FOUND) break;
        if (status != MFX_ERR_NONE) return status;
        MFXDispReleaseImplDescription(value->loader, description);
        mfxHDL device = nullptr;
        status = MFXEnumImplementations(value->loader, i, MFX_IMPLCAPS_DEVICE_ID_EXTENDED, &device);
        if (status == MFX_ERR_NONE && device) {
            auto id = static_cast<mfxExtendedDeviceId*>(device);
            bool match = id->LUIDValid && memcmp(id->DeviceLUID, &luid, sizeof(luid)) == 0;
            if (match) snprintf(value->description, sizeof(value->description), "%s", id->DeviceName);
            MFXDispReleaseImplDescription(value->loader, device);
            if (match) { *index = i; return MFX_ERR_NONE; }
        } else {
            unverified += 1;
            unverified_index = i;
        }
    }
    if (unverified == 1) {
        // HARDWARE-CHECK: a Media SDK 1.x runtime cannot report its adapter.
        snprintf(value->description, sizeof(value->description), "adapter unverified (no extended device ID)");
        *index = unverified_index;
        return MFX_ERR_NONE;
    }
    note(text, capacity, unverified ? "several Intel runtimes without adapter identity" : "no Intel runtime on this adapter");
    return kUnsupported;
}

static mfxU16 align16(uint32_t value) { return static_cast<mfxU16>((value + 15) & ~15u); }

static bool free_bitstream(FlinchQsv* value, uint32_t* found) {
    for (uint32_t b = 0; b < kBitstreams; ++b)
        if (!value->bitstream_busy[b] && static_cast<int>(b) != value->read) { *found = b; return true; }
    return false;
}

static void enqueue(FlinchQsv* value, mfxSyncPoint sync, uint32_t bitstream) {
    value->queue[(value->head + value->count) % kBitstreams] = {sync, bitstream};
    value->count += 1;
    value->bitstream_busy[bitstream] = true;
}

extern "C" {
int flinch_qsv_open(void* device, int64_t luid, const FlinchQsvConfig* config, FlinchQsv** output, int* stage,
                    char* text, size_t capacity) {
    *output = nullptr;
    if (text && capacity) text[0] = 0;
    *stage = kStageLoad;
    if (config->width > 0xffff || config->height > 0xffff || config->fps == 0) return kInvalid;
    auto value = new (std::nothrow) FlinchQsv;
    if (!value) return MFX_ERR_MEMORY_ALLOC;
    value->loader = MFXLoad();
    if (!value->loader) { delete value; return MFX_ERR_MEMORY_ALLOC; }

    *stage = kStageFilter;
    mfxStatus status = filter(value->loader, "mfxImplDescription.Impl", MFX_IMPL_TYPE_HARDWARE);
    if (status == MFX_ERR_NONE)
        status = filter(value->loader, "mfxImplDescription.AccelerationMode", MFX_ACCEL_MODE_VIA_D3D11);
    if (status == MFX_ERR_NONE)
        status = filter(value->loader, "mfxImplDescription.mfxEncoderDescription.encoder.CodecID", MFX_CODEC_AVC);
    if (status != MFX_ERR_NONE) { delete value; return status; }

    *stage = kStageAdapter;
    mfxU32 index = 0;
    int chosen = choose(value, luid, &index, text, capacity);
    if (chosen != MFX_ERR_NONE) { delete value; return chosen; }

    *stage = kStageSession;
    status = MFXCreateSession(value->loader, index, &value->session);
    if (status != MFX_ERR_NONE) { value->session = nullptr; delete value; return status; }
    *stage = kStageHandle;
    status = MFXVideoCORE_SetHandle(value->session, MFX_HANDLE_D3D11_DEVICE, device);
    if (status != MFX_ERR_NONE) { delete value; return status; }
    *stage = kStageAllocator;
    value->allocator.pthis = value;
    value->allocator.Alloc = alloc_frames;
    value->allocator.Lock = lock_frame;
    value->allocator.Unlock = unlock_frame;
    value->allocator.GetHDL = frame_handle;
    value->allocator.Free = free_frames;
    status = MFXVideoCORE_SetFrameAllocator(value->session, &value->allocator);
    if (status != MFX_ERR_NONE) { delete value; return status; }

    *stage = kStageQuery;
    mfxVideoParam par{};
    par.IOPattern = MFX_IOPATTERN_IN_VIDEO_MEMORY;
    par.AsyncDepth = 4;
    par.mfx.CodecId = MFX_CODEC_AVC;
    par.mfx.CodecProfile = MFX_PROFILE_AVC_HIGH;
    par.mfx.TargetUsage = MFX_TARGETUSAGE_BALANCED;
    par.mfx.GopPicSize = static_cast<mfxU16>(config->gop);
    par.mfx.GopRefDist = 1;  // no B-frames: the muxer requires DTS == PTS
    par.mfx.IdrInterval = 0;  // every I-frame is an IDR
    if (config->constant_qp) {
        par.mfx.RateControlMethod = MFX_RATECONTROL_CQP;
        par.mfx.QPI = static_cast<mfxU16>(config->qp_i);
        par.mfx.QPP = static_cast<mfxU16>(config->qp_p);
        par.mfx.QPB = static_cast<mfxU16>(config->qp_p);
    } else {
        uint32_t kbps = config->bitrate / 1000;
        uint32_t multiplier = kbps / 0xffff + 1;
        par.mfx.RateControlMethod = MFX_RATECONTROL_VBR;
        par.mfx.BRCParamMultiplier = static_cast<mfxU16>(multiplier);
        par.mfx.TargetKbps = static_cast<mfxU16>(kbps / multiplier);
        par.mfx.MaxKbps = static_cast<mfxU16>(kbps / multiplier);
    }
    mfxFrameInfo& info = par.mfx.FrameInfo;
    info.FourCC = MFX_FOURCC_NV12;
    info.ChromaFormat = MFX_CHROMAFORMAT_YUV420;
    info.PicStruct = MFX_PICSTRUCT_PROGRESSIVE;
    // HARDWARE-CHECK: Intel wants 16-aligned frame sizes; the engine's textures are exactly the
    // crop size, which may need aligned allocation for sizes such as 1366x768.
    info.Width = align16(config->width);
    info.Height = align16(config->height);
    info.CropW = static_cast<mfxU16>(config->width);
    info.CropH = static_cast<mfxU16>(config->height);
    info.FrameRateExtN = config->fps;
    info.FrameRateExtD = 1;
    status = MFXVideoENCODE_Query(value->session, &par, &par);
    if (status == MFX_WRN_PARTIAL_ACCELERATION) { note(text, capacity, "partial acceleration (software)"); delete value; return kUnsupported; }
    if (status < MFX_ERR_NONE) { delete value; return status; }

    *stage = kStageInitialize;
    status = MFXVideoENCODE_Init(value->session, &par);
    if (status == MFX_WRN_PARTIAL_ACCELERATION) { note(text, capacity, "partial acceleration (software)"); delete value; return kUnsupported; }
    if (status < MFX_ERR_NONE) { delete value; return status; }
    value->initialized = true;

    *stage = kStageParameters;
    mfxU8 sps[512]{}, pps[512]{};
    mfxExtCodingOptionSPSPPS headers{};
    headers.Header.BufferId = MFX_EXTBUFF_CODING_OPTION_SPSPPS;
    headers.Header.BufferSz = sizeof(headers);
    headers.SPSBuffer = sps;
    headers.SPSBufSize = sizeof(sps);
    headers.PPSBuffer = pps;
    headers.PPSBufSize = sizeof(pps);
    mfxExtBuffer* extensions[] = {&headers.Header};
    mfxVideoParam actual{};
    actual.ExtParam = extensions;
    actual.NumExtParam = 1;
    status = MFXVideoENCODE_GetVideoParam(value->session, &actual);
    if (status < MFX_ERR_NONE) { delete value; return status; }
    // The runtime may adjust parameters; B-frames or another rate control would break the contract.
    if (actual.mfx.GopRefDist > 1 || actual.mfx.RateControlMethod != par.mfx.RateControlMethod) {
        note(text, capacity, "runtime changed B-frames or rate control");
        delete value;
        return kUnsupported;
    }
    if (headers.SPSBufSize + headers.PPSBufSize > sizeof(value->extra) || headers.SPSBufSize == 0) {
        delete value;
        return kInvalid;
    }
    memcpy(value->extra, sps, headers.SPSBufSize);
    memcpy(value->extra + headers.SPSBufSize, pps, headers.PPSBufSize);
    value->extra_size = headers.SPSBufSize + headers.PPSBufSize;

    *stage = kStageBitstream;
    uint32_t multiplier = actual.mfx.BRCParamMultiplier ? actual.mfx.BRCParamMultiplier : 1;
    size_t size = static_cast<size_t>(actual.mfx.BufferSizeInKB) * 1000 * multiplier;
    size_t floor = static_cast<size_t>(config->width) * config->height * 3 / 2;
    if (size < floor) size = floor;
    for (uint32_t b = 0; b < kBitstreams; ++b) {
        try { value->buffers[b].resize(size); } catch (...) { delete value; return MFX_ERR_MEMORY_ALLOC; }
        value->bitstreams[b].Data = value->buffers[b].data();
        value->bitstreams[b].MaxLength = static_cast<mfxU32>(size);
    }
    for (uint32_t slot = 0; slot < kSlots; ++slot) {
        value->surfaces[slot].Info = actual.mfx.FrameInfo;
        value->surfaces[slot].Data.MemId = &value->handles[slot];
    }
    *stage = 0;
    *output = value;
    return MFX_ERR_NONE;
}

// Read-only inventory for doctor: every hardware runtime the dispatcher finds, without a session.
int flinch_qsv_inventory(char* text, size_t capacity, uint32_t* count) {
    *count = 0;
    if (text && capacity) text[0] = 0;
    mfxLoader loader = MFXLoad();
    if (!loader) return MFX_ERR_MEMORY_ALLOC;
    mfxStatus status = filter(loader, "mfxImplDescription.Impl", MFX_IMPL_TYPE_HARDWARE);
    size_t used = 0;
    for (mfxU32 i = 0; status == MFX_ERR_NONE; ++i) {
        mfxHDL handle = nullptr;
        mfxStatus found = MFXEnumImplementations(loader, i, MFX_IMPLCAPS_IMPLDESCSTRUCTURE, &handle);
        if (found == MFX_ERR_NOT_FOUND) break;
        if (found != MFX_ERR_NONE) { status = found; break; }
        auto description = static_cast<mfxImplDescription*>(handle);
        if (text && used < capacity) {
            int written = snprintf(text + used, capacity - used, "%s%s (API %u.%u, vendor 0x%04x)",
                                   *count ? "; " : "", description->ImplName, description->ApiVersion.Major,
                                   description->ApiVersion.Minor, description->VendorID);
            if (written > 0) used += static_cast<size_t>(written);
        }
        MFXDispReleaseImplDescription(loader, handle);
        *count += 1;
    }
    MFXUnload(loader);
    return status;
}

int flinch_qsv_describe(FlinchQsv* value, char* text, size_t capacity) {
    mfxVersion version{};
    mfxStatus status = MFXQueryVersion(value->session, &version);
    if (status != MFX_ERR_NONE) return status;
    snprintf(text, capacity, "%s, runtime API %u.%u", value->description, version.Major, version.Minor);
    return MFX_ERR_NONE;
}

int flinch_qsv_extra(FlinchQsv* value, uint8_t* output, size_t capacity, size_t* size) {
    *size = value->extra_size;
    if (capacity < value->extra_size) return kInputFull;
    memcpy(output, value->extra, value->extra_size);
    return MFX_ERR_NONE;
}

int flinch_qsv_busy(FlinchQsv* value, uint32_t slot) {
    // HARDWARE-CHECK: the runtime holds Data.Locked above zero while it reads the surface.
    return slot >= kSlots || value->surfaces[slot].Data.Locked != 0;
}

int flinch_qsv_submit(FlinchQsv* value, void* texture, uint32_t slot, int64_t pts, int64_t duration, int keyframe) {
    if (slot >= kSlots || value->surfaces[slot].Data.Locked != 0 || value->timing_count >= kTimings)
        return kInputFull;
    if (value->eos || pts < 0 || duration <= 0) return kInvalid;
    uint32_t bitstream = 0;
    if (!free_bitstream(value, &bitstream)) return kInputFull;
    value->handles[slot].first = texture;
    value->handles[slot].second = nullptr;  // subresource 0
    mfxEncodeCtrl control{};
    if (keyframe) control.FrameType = MFX_FRAMETYPE_I | MFX_FRAMETYPE_IDR | MFX_FRAMETYPE_REF;
    mfxSyncPoint sync{};
    mfxStatus status = MFXVideoENCODE_EncodeFrameAsync(value->session, keyframe ? &control : nullptr,
                                                       &value->surfaces[slot], &value->bitstreams[bitstream], &sync);
    if (status == MFX_WRN_DEVICE_BUSY) return kInputFull;  // not accepted; retried later
    if (status < MFX_ERR_NONE && status != MFX_ERR_MORE_DATA) return status;
    value->timings[(value->timing_head + value->timing_count) % kTimings] = {pts, duration};
    value->timing_count += 1;
    // MORE_DATA: accepted and buffered; its output arrives with a later call.
    if (status != MFX_ERR_MORE_DATA && sync) enqueue(value, sync, bitstream);
    return MFX_ERR_NONE;
}

int flinch_qsv_poll(FlinchQsv* value, const uint8_t** data, size_t* size, int64_t* pts, int64_t* duration,
                    int* keyframe) {
    if (value->read >= 0) {
        mfxBitstream& done = value->bitstreams[value->read];
        done.DataLength = 0;
        done.DataOffset = 0;
        value->read = -1;
    }
    if (value->count == 0) return value->eos ? kEof : kRepeat;
    FlinchQsv::Pending pending = value->queue[value->head];
    mfxStatus status = MFXVideoCORE_SyncOperation(value->session, pending.sync, 0);
    if (status == MFX_WRN_IN_EXECUTION) return kRepeat;
    if (status < MFX_ERR_NONE) return status;
    if (value->timing_count == 0) return kInvalid;
    value->head = (value->head + 1) % kBitstreams;
    value->count -= 1;
    value->bitstream_busy[pending.bitstream] = false;
    value->read = static_cast<int>(pending.bitstream);
    FlinchQsv::Timing timing = value->timings[value->timing_head];
    value->timing_head = (value->timing_head + 1) % kTimings;
    value->timing_count -= 1;
    mfxBitstream& out = value->bitstreams[pending.bitstream];
    *data = out.Data + out.DataOffset;
    *size = out.DataLength;
    *pts = timing.pts;
    *duration = timing.duration;
    *keyframe = (out.FrameType & (MFX_FRAMETYPE_IDR | MFX_FRAMETYPE_I)) != 0;
    return MFX_ERR_NONE;
}

int flinch_qsv_drain(FlinchQsv* value) {
    while (!value->eos) {
        uint32_t bitstream = 0;
        if (!free_bitstream(value, &bitstream)) return kInputFull;  // poll, then drain again
        mfxSyncPoint sync{};
        mfxStatus status = MFXVideoENCODE_EncodeFrameAsync(value->session, nullptr, nullptr,
                                                           &value->bitstreams[bitstream], &sync);
        if (status == MFX_ERR_MORE_DATA) { value->eos = true; break; }
        if (status == MFX_WRN_DEVICE_BUSY) return kInputFull;
        if (status < MFX_ERR_NONE) return status;
        if (sync) enqueue(value, sync, bitstream);
    }
    return MFX_ERR_NONE;
}

void flinch_qsv_close(FlinchQsv* value) { delete value; }
}
