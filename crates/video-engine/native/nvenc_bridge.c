// Narrow C ABI over the unmodified NVIDIA NVENC public header. MIT, Flinch authors.
//
// Follows nvEncodeAPI.h 12.2 and NVIDIA's documented async-mode D3D11 flow.
#include <windows.h>
#include <stdint.h>
#include <string.h>
#include "nvenc/nvEncodeAPI.h"

// Pool slots; must equal the engine's limits::MAX_SURFACES (checked by a Rust test).
#define FLINCH_NVENC_SLOTS 16
// Bridge results outside NVENCSTATUS (which ends below 64).
#define FLINCH_NVENC_REPEAT 1000
#define FLINCH_NVENC_EOF 1001
#define FLINCH_NVENC_INPUT_FULL 1002
#define FLINCH_NVENC_UNSUPPORTED 1003
#define FLINCH_NVENC_INVALID 1004

// Open stages, reported with a failure so a remote driver log names the step.
enum {
    STAGE_LOAD = 1, STAGE_VERSION, STAGE_INSTANCE, STAGE_SESSION, STAGE_CODEC, STAGE_CAPS,
    STAGE_PRESET, STAGE_INITIALIZE, STAGE_BITSTREAM, STAGE_EVENT, STAGE_SEQUENCE
};

typedef struct FlinchNvencConfig {
    uint32_t width, height, fps;
    uint32_t constant_qp; // 1: constant QP (qp_i/qp_p), 0: VBR at bitrate
    uint32_t qp_i, qp_p, bitrate, gop;
} FlinchNvencConfig;

typedef struct FlinchNvenc {
    HMODULE module;
    NV_ENCODE_API_FUNCTION_LIST api;
    void* encoder;
    int async;
    uint32_t width, height;
    void* texture[FLINCH_NVENC_SLOTS];
    NV_ENC_REGISTERED_PTR registered[FLINCH_NVENC_SLOTS];
    NV_ENC_INPUT_PTR mapped[FLINCH_NVENC_SLOTS];
    NV_ENC_OUTPUT_PTR bitstream[FLINCH_NVENC_SLOTS];
    HANDLE events[FLINCH_NVENC_SLOTS];
    // Async mode only: the driver rejects an EOS picture without its own completion event.
    HANDLE eos_event;
    // Submitted slots in encode order; outputs are retrieved in the same order.
    uint32_t queue[FLINCH_NVENC_SLOTS];
    uint32_t head, count;
    int locked; // slot whose bitstream the caller is reading, or -1
    int eos;
    uint8_t extra[1024];
    uint32_t extra_size;
} FlinchNvenc;

static void message(FlinchNvenc* value, char* text, size_t capacity) {
    if (!text || capacity == 0) return;
    text[0] = 0;
    if (value && value->encoder && value->api.nvEncGetLastErrorString) {
        const char* last = value->api.nvEncGetLastErrorString(value->encoder);
        if (last) strncpy_s(text, capacity, last, _TRUNCATE);
    }
}

static void release_slot(FlinchNvenc* value, uint32_t slot) {
    if (value->mapped[slot]) {
        value->api.nvEncUnmapInputResource(value->encoder, value->mapped[slot]);
        value->mapped[slot] = NULL;
    }
}

static void unlock(FlinchNvenc* value) {
    if (value->locked >= 0) {
        uint32_t slot = (uint32_t)value->locked;
        value->api.nvEncUnlockBitstream(value->encoder, value->bitstream[slot]);
        release_slot(value, slot);
        value->locked = -1;
    }
}

static void unregister_event(FlinchNvenc* value, HANDLE handle) {
    NV_ENC_EVENT_PARAMS event = {0};
    event.version = NV_ENC_EVENT_PARAMS_VER;
    event.completionEvent = handle;
    value->api.nvEncUnregisterAsyncEvent(value->encoder, &event);
}

static void destroy(FlinchNvenc* value) {
    if (!value) return;
    if (value->encoder) {
        unlock(value);
        if (value->eos_event) {
            unregister_event(value, value->eos_event);
            CloseHandle(value->eos_event);
        }
        for (uint32_t slot = 0; slot < FLINCH_NVENC_SLOTS; ++slot) {
            release_slot(value, slot);
            if (value->registered[slot]) value->api.nvEncUnregisterResource(value->encoder, value->registered[slot]);
            if (value->bitstream[slot]) value->api.nvEncDestroyBitstreamBuffer(value->encoder, value->bitstream[slot]);
            if (value->events[slot]) {
                if (value->async) unregister_event(value, value->events[slot]);
                CloseHandle(value->events[slot]);
            }
        }
        value->api.nvEncDestroyEncoder(value->encoder);
    }
    if (value->module) FreeLibrary(value->module);
    HeapFree(GetProcessHeap(), 0, value);
}

static HMODULE load(void) {
    return LoadLibraryExW(L"nvEncodeAPI64.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
}

int flinch_nvenc_version(uint32_t* version) {
    HMODULE module = load();
    if (!module) return -(int)GetLastError();
    typedef NVENCSTATUS(NVENCAPI * Max)(uint32_t*);
    Max query = (Max)GetProcAddress(module, "NvEncodeAPIGetMaxSupportedVersion");
    int result = query ? (int)query(version) : -(int)ERROR_PROC_NOT_FOUND;
    FreeLibrary(module);
    return result;
}

static int caps(FlinchNvenc* value, NV_ENC_CAPS query, int* out) {
    NV_ENC_CAPS_PARAM param = {0};
    param.version = NV_ENC_CAPS_PARAM_VER;
    param.capsToQuery = query;
    return (int)value->api.nvEncGetEncodeCaps(value->encoder, NV_ENC_CODEC_H264_GUID, &param, out);
}

// Creates and registers one completion event; on failure nothing is left to unregister.
static int register_event(FlinchNvenc* value, HANDLE* handle) {
    HANDLE created = CreateEventW(NULL, FALSE, FALSE, NULL);
    if (!created) return -(int)GetLastError();
    NV_ENC_EVENT_PARAMS event = {0};
    event.version = NV_ENC_EVENT_PARAMS_VER;
    event.completionEvent = created;
    NVENCSTATUS status = value->api.nvEncRegisterAsyncEvent(value->encoder, &event);
    if (status != NV_ENC_SUCCESS) {
        CloseHandle(created);
        return (int)status;
    }
    *handle = created;
    return NV_ENC_SUCCESS;
}

// A slot's bitstream buffer (and async completion event), created on the slot's first use so an
// encoder allocates only the outputs it uses: the three-frame self-test no longer allocates all
// sixteen. On a Pascal-generation card at 1440p this cut peak private memory by 72–82 MB and
// encoder open by 20–30 ms. Idempotent per slot.
static int create_output(FlinchNvenc* value, uint32_t slot, int* stage) {
    if (!value->bitstream[slot]) {
        *stage = STAGE_BITSTREAM;
        NV_ENC_CREATE_BITSTREAM_BUFFER buffer = {0};
        buffer.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        NVENCSTATUS status = value->api.nvEncCreateBitstreamBuffer(value->encoder, &buffer);
        if (status != NV_ENC_SUCCESS) return (int)status;
        value->bitstream[slot] = buffer.bitstreamBuffer;
    }
    if (value->async && !value->events[slot]) {
        *stage = STAGE_EVENT;
        return register_event(value, &value->events[slot]);
    }
    return NV_ENC_SUCCESS;
}

int flinch_nvenc_open(void* device, const FlinchNvencConfig* config, FlinchNvenc** output, int* stage,
                      char* text, size_t capacity) {
    *output = NULL;
    *stage = STAGE_LOAD;
    if (text && capacity) text[0] = 0;
    FlinchNvenc* value = (FlinchNvenc*)HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(FlinchNvenc));
    if (!value) return NV_ENC_ERR_OUT_OF_MEMORY;
    value->locked = -1;
    value->width = config->width;
    value->height = config->height;
    value->module = load();
    if (!value->module) { int e = -(int)GetLastError(); destroy(value); return e; }

    *stage = STAGE_VERSION;
    typedef NVENCSTATUS(NVENCAPI * Max)(uint32_t*);
    typedef NVENCSTATUS(NVENCAPI * Create)(NV_ENCODE_API_FUNCTION_LIST*);
    Max maximum = (Max)GetProcAddress(value->module, "NvEncodeAPIGetMaxSupportedVersion");
    Create create = (Create)GetProcAddress(value->module, "NvEncodeAPICreateInstance");
    if (!maximum || !create) { destroy(value); return -(int)ERROR_PROC_NOT_FOUND; }
    uint32_t supported = 0;
    NVENCSTATUS status = maximum(&supported);
    if (status != NV_ENC_SUCCESS) { destroy(value); return (int)status; }
    // The runtime reports (major << 4) | minor; it must be at least this header's API.
    if (supported < ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION)) {
        destroy(value);
        return NV_ENC_ERR_INVALID_VERSION;
    }

    *stage = STAGE_INSTANCE;
    value->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    status = create(&value->api);
    if (status != NV_ENC_SUCCESS) { destroy(value); return (int)status; }

    *stage = STAGE_SESSION;
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS session = {0};
    session.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    session.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    session.device = device;
    session.apiVersion = NVENCAPI_VERSION;
    status = value->api.nvEncOpenEncodeSessionEx(&session, &value->encoder);
    if (status != NV_ENC_SUCCESS) {
        // A failed open still hands back a session that must be destroyed (NVIDIA samples do so).
        destroy(value);
        return (int)status;
    }

    *stage = STAGE_CODEC;
    uint32_t count = 0, found = 0;
    GUID guids[16];
    status = value->api.nvEncGetEncodeGUIDCount(value->encoder, &count);
    if (status == NV_ENC_SUCCESS)
        status = value->api.nvEncGetEncodeGUIDs(value->encoder, guids, count < 16 ? count : 16, &found);
    if (status != NV_ENC_SUCCESS) { message(value, text, capacity); destroy(value); return (int)status; }
    int h264 = 0;
    for (uint32_t i = 0; i < found; ++i) h264 |= IsEqualGUID(&guids[i], &NV_ENC_CODEC_H264_GUID);
    if (!h264) { destroy(value); return FLINCH_NVENC_UNSUPPORTED; }

    *stage = STAGE_CAPS;
    int width_max = 0, height_max = 0, async = 0;
    status = (NVENCSTATUS)caps(value, NV_ENC_CAPS_WIDTH_MAX, &width_max);
    if (status == NV_ENC_SUCCESS) status = (NVENCSTATUS)caps(value, NV_ENC_CAPS_HEIGHT_MAX, &height_max);
    if (status == NV_ENC_SUCCESS) status = (NVENCSTATUS)caps(value, NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT, &async);
    if (status != NV_ENC_SUCCESS) { message(value, text, capacity); destroy(value); return (int)status; }
    if ((int)config->width > width_max || (int)config->height > height_max) {
        destroy(value);
        return FLINCH_NVENC_UNSUPPORTED;
    }
    value->async = async != 0;

    *stage = STAGE_PRESET;
    NV_ENC_PRESET_CONFIG preset = {0};
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    status = value->api.nvEncGetEncodePresetConfigEx(value->encoder, NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P4_GUID,
                                                     NV_ENC_TUNING_INFO_LOW_LATENCY, &preset);
    if (status != NV_ENC_SUCCESS) { message(value, text, capacity); destroy(value); return (int)status; }
    NV_ENC_CONFIG encode = preset.presetCfg;
    encode.version = NV_ENC_CONFIG_VER;
    encode.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
    encode.gopLength = config->gop;
    encode.frameIntervalP = 1; // no B-frames: the muxer requires DTS == PTS
    encode.rcParams.multiPass = NV_ENC_MULTI_PASS_DISABLED;
    if (config->constant_qp) {
        encode.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
        encode.rcParams.constQP.qpIntra = config->qp_i;
        encode.rcParams.constQP.qpInterP = config->qp_p;
        encode.rcParams.constQP.qpInterB = config->qp_p;
    } else {
        encode.rcParams.rateControlMode = NV_ENC_PARAMS_RC_VBR;
        encode.rcParams.averageBitRate = config->bitrate;
        encode.rcParams.maxBitRate = config->bitrate;
    }
    encode.encodeCodecConfig.h264Config.idrPeriod = config->gop;
    encode.encodeCodecConfig.h264Config.repeatSPSPPS = 1; // every IDR stays decodable on its own

    *stage = STAGE_INITIALIZE;
    NV_ENC_INITIALIZE_PARAMS init = {0};
    init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init.encodeGUID = NV_ENC_CODEC_H264_GUID;
    init.presetGUID = NV_ENC_PRESET_P4_GUID;
    init.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;
    init.encodeWidth = init.darWidth = init.maxEncodeWidth = config->width;
    init.encodeHeight = init.darHeight = init.maxEncodeHeight = config->height;
    init.frameRateNum = config->fps;
    init.frameRateDen = 1;
    init.enablePTD = 1;
    init.enableEncodeAsync = value->async ? 1 : 0;
    init.encodeConfig = &encode;
    status = value->api.nvEncInitializeEncoder(value->encoder, &init);
    if (status != NV_ENC_SUCCESS) { message(value, text, capacity); destroy(value); return (int)status; }

    // Slot 0's output is created here so an unusable driver fails at Open; the others are created on
    // first use (see create_output).
    int failed_stage = 0;
    int result = create_output(value, 0, &failed_stage);
    if (result == NV_ENC_SUCCESS && value->async) {
        failed_stage = STAGE_EVENT;
        result = register_event(value, &value->eos_event);
    }
    if (result != NV_ENC_SUCCESS) {
        *stage = failed_stage;
        message(value, text, capacity);
        destroy(value);
        return result;
    }

    *stage = STAGE_SEQUENCE;
    NV_ENC_SEQUENCE_PARAM_PAYLOAD sequence = {0};
    sequence.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
    sequence.inBufferSize = sizeof(value->extra);
    sequence.spsppsBuffer = value->extra;
    sequence.outSPSPPSPayloadSize = &value->extra_size;
    status = value->api.nvEncGetSequenceParams(value->encoder, &sequence);
    if (status != NV_ENC_SUCCESS) { message(value, text, capacity); destroy(value); return (int)status; }
    *stage = 0;
    *output = value;
    return NV_ENC_SUCCESS;
}

int flinch_nvenc_extra(FlinchNvenc* value, uint8_t* output, size_t capacity, size_t* size) {
    *size = value->extra_size;
    if (capacity < value->extra_size) return FLINCH_NVENC_INPUT_FULL;
    memcpy(output, value->extra, value->extra_size);
    return NV_ENC_SUCCESS;
}

int flinch_nvenc_busy(FlinchNvenc* value, uint32_t slot) {
    return slot >= FLINCH_NVENC_SLOTS || value->mapped[slot] != NULL;
}

int flinch_nvenc_submit(FlinchNvenc* value, void* texture, uint32_t slot, int64_t pts, int64_t duration,
                        int keyframe) {
    if (slot >= FLINCH_NVENC_SLOTS || value->mapped[slot] || value->count >= FLINCH_NVENC_SLOTS)
        return FLINCH_NVENC_INPUT_FULL;
    if (value->eos || pts < 0 || duration <= 0) return FLINCH_NVENC_INVALID;
    int unused_stage = 0;
    int created = create_output(value, slot, &unused_stage);
    if (created != NV_ENC_SUCCESS) return created;
    NVENCSTATUS status;
    // Each pool slot keeps one texture for the session; register it once.
    if (value->texture[slot] != texture) {
        if (value->registered[slot]) {
            status = value->api.nvEncUnregisterResource(value->encoder, value->registered[slot]);
            value->registered[slot] = NULL;
            if (status != NV_ENC_SUCCESS) return (int)status;
        }
        NV_ENC_REGISTER_RESOURCE resource = {0};
        resource.version = NV_ENC_REGISTER_RESOURCE_VER;
        resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        resource.width = value->width;
        resource.height = value->height;
        resource.resourceToRegister = texture;
        resource.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
        resource.bufferUsage = NV_ENC_INPUT_IMAGE;
        status = value->api.nvEncRegisterResource(value->encoder, &resource);
        if (status != NV_ENC_SUCCESS) return (int)status;
        value->registered[slot] = resource.registeredResource;
        value->texture[slot] = texture;
    }
    NV_ENC_MAP_INPUT_RESOURCE map = {0};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = value->registered[slot];
    status = value->api.nvEncMapInputResource(value->encoder, &map);
    if (status != NV_ENC_SUCCESS) return (int)status;
    value->mapped[slot] = map.mappedResource;

    NV_ENC_PIC_PARAMS picture = {0};
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.inputWidth = value->width;
    picture.inputHeight = value->height;
    picture.inputBuffer = map.mappedResource;
    picture.bufferFmt = map.mappedBufferFmt;
    picture.outputBitstream = value->bitstream[slot];
    picture.completionEvent = value->async ? value->events[slot] : NULL;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    picture.inputTimeStamp = (uint64_t)pts;
    picture.inputDuration = (uint64_t)duration;
    if (keyframe) picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    status = value->api.nvEncEncodePicture(value->encoder, &picture);
    if (status == NV_ENC_ERR_ENCODER_BUSY) {
        // Documented as transient; the caller retries this slot later.
        release_slot(value, slot);
        return FLINCH_NVENC_INPUT_FULL;
    }
    if (status != NV_ENC_SUCCESS) {
        // NEED_MORE_INPUT cannot occur without B-frames; any other result is a failure.
        release_slot(value, slot);
        return (int)status;
    }
    value->queue[(value->head + value->count) % FLINCH_NVENC_SLOTS] = slot;
    value->count += 1;
    return NV_ENC_SUCCESS;
}

int flinch_nvenc_poll(FlinchNvenc* value, const uint8_t** data, size_t* size, int64_t* pts, int64_t* duration,
                      int* keyframe) {
    // The previous packet stays readable until this call, as with AMF.
    unlock(value);
    if (value->count == 0) {
        if (!value->eos) return FLINCH_NVENC_REPEAT;
        if (value->eos == 2 || !value->async) return FLINCH_NVENC_EOF;
        // Async: the stream ends once the driver signals the EOS event; callers bound this wait.
        DWORD wait = WaitForSingleObject(value->eos_event, 0);
        if (wait == WAIT_TIMEOUT) return FLINCH_NVENC_REPEAT;
        if (wait != WAIT_OBJECT_0) return -(int)GetLastError();
        value->eos = 2;
        return FLINCH_NVENC_EOF;
    }
    uint32_t slot = value->queue[value->head];
    if (value->async) {
        DWORD wait = WaitForSingleObject(value->events[slot], 0);
        if (wait == WAIT_TIMEOUT) return FLINCH_NVENC_REPEAT;
        if (wait != WAIT_OBJECT_0) return -(int)GetLastError();
    }
    // Async: completed, so this returns at once. Sync: blocks until this frame is encoded.
    NV_ENC_LOCK_BITSTREAM lock = {0};
    lock.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock.outputBitstream = value->bitstream[slot];
    NVENCSTATUS status = value->api.nvEncLockBitstream(value->encoder, &lock);
    if (status != NV_ENC_SUCCESS) return (int)status;
    value->head = (value->head + 1) % FLINCH_NVENC_SLOTS;
    value->count -= 1;
    value->locked = (int)slot;
    *data = (const uint8_t*)lock.bitstreamBufferPtr;
    *size = lock.bitstreamSizeInBytes;
    *pts = (int64_t)lock.outputTimeStamp;
    *duration = (int64_t)lock.outputDuration;
    *keyframe = lock.pictureType == NV_ENC_PIC_TYPE_IDR || lock.pictureType == NV_ENC_PIC_TYPE_I;
    return NV_ENC_SUCCESS;
}

int flinch_nvenc_drain(FlinchNvenc* value) {
    if (value->eos) return NV_ENC_SUCCESS;
    NV_ENC_PIC_PARAMS picture = {0};
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
    // Confirmed on Pascal-generation hardware: async mode answers NV_ENC_ERR_INVALID_PARAM to an
    // EOS without a completion event. Without B-frames, EOS produces no extra output.
    picture.completionEvent = value->async ? value->eos_event : NULL;
    NVENCSTATUS status = value->api.nvEncEncodePicture(value->encoder, &picture);
    if (status != NV_ENC_SUCCESS) return (int)status;
    value->eos = 1;
    return NV_ENC_SUCCESS;
}

void flinch_nvenc_close(FlinchNvenc* value) { destroy(value); }
