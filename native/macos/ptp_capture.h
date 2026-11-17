// C ABI between the Rust host and the macOS capture/encode layer
// (ScreenCaptureKit + VideoToolbox). Everything here is called from Rust.
#pragma once
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
    PTP_SOURCE_DISPLAY = 0,
    PTP_SOURCE_WINDOW = 1,
    PTP_SOURCE_TEST_PATTERN = 2,
};

enum {
    PTP_AUDIO_NONE = 0,
    PTP_AUDIO_SYSTEM_EXCLUDING = 1, // all system audio except `excluded_bundles`
    PTP_AUDIO_SINGLE_APP = 2,       // only `audio_app_bundle`
};

typedef void (*ptp_video_cb)(void *ctx, const uint8_t *annexb, size_t len, int64_t pts_us, int keyframe);
typedef void (*ptp_audio_cb)(void *ctx, const float *interleaved_stereo, size_t frames, uint32_t sample_rate,
                             int64_t pts_us, int is_mic);
typedef void (*ptp_error_cb)(void *ctx, const char *message);

typedef struct {
    uint32_t source_kind;
    uint32_t source_id; // CGDirectDisplayID or CGWindowID
    uint32_t max_width;
    uint32_t max_height;
    uint32_t fps;
    uint32_t bitrate_kbps;
    uint32_t audio_mode;
    const char *audio_app_bundle; // PTP_AUDIO_SINGLE_APP
    const char *excluded_bundles; // newline separated, PTP_AUDIO_SYSTEM_EXCLUDING
    uint32_t capture_mic;         // macOS 15+
    void *ctx;
    ptp_video_cb on_video;
    ptp_audio_cb on_audio;
    ptp_error_cb on_error;
} ptp_config;

// Returns malloc'd JSON: {"displays":[...],"windows":[...],"apps":[...]} or NULL.
char *ptp_list_sources_json(void);
void ptp_free(void *ptr);

// Thumbnail as RGBA8 (malloc'd, free with ptp_free). Returns 0 on success.
// Requires macOS 14+ (SCScreenshotManager); returns -1 otherwise.
int ptp_thumbnail(uint32_t kind, uint32_t id, uint32_t max_w, uint32_t max_h, uint8_t **out_rgba,
                  uint32_t *out_w, uint32_t *out_h);

int ptp_has_screen_access(void);
int ptp_request_screen_access(void);
int ptp_mic_supported(void);

// Starts capture + encode. Returns opaque handle or NULL (error text in err).
void *ptp_start(const ptp_config *cfg, uint32_t *out_width, uint32_t *out_height, char *err, size_t err_len);
void ptp_request_keyframe(void *handle);
void ptp_set_bitrate(void *handle, uint32_t kbps);
// Stops everything. After it returns no callback will fire again.
void ptp_stop(void *handle);

#ifdef __cplusplus
}
#endif
