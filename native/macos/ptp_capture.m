// macOS capture + hardware encode layer for the PTP Share host.
//
// - ScreenCaptureKit captures a display/window (video stream) and, separately,
//   system / per-app audio (+ optional microphone on macOS 15+) on a second
//   stream, so audio filtering never hides windows from the video.
// - VideoToolbox encodes H.264 (constrained high, low-latency rate control),
//   output is converted to Annex-B and handed to Rust together with PCM audio.

#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreMedia/CoreMedia.h>
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <VideoToolbox/VideoToolbox.h>
#include <mach/mach_time.h>
#include <stdatomic.h>

#include "ptp_capture.h"

// ───────────────────────────── helpers ─────────────────────────────

static void set_err(char *err, size_t len, NSString *msg) {
    if (!err || len == 0) return;
    const char *s = msg.UTF8String ?: "unknown error";
    strlcpy(err, s, len);
}

static int64_t host_time_us(void) {
    CMTime t = CMClockGetTime(CMClockGetHostTimeClock());
    return (int64_t)(CMTimeGetSeconds(t) * 1e6);
}

static SCShareableContent *fetch_content(NSError **outErr) {
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block SCShareableContent *result = nil;
    __block NSError *error = nil;
    [SCShareableContent getShareableContentExcludingDesktopWindows:YES
                                               onScreenWindowsOnly:YES
                                                 completionHandler:^(SCShareableContent *c, NSError *e) {
                                                     result = c;
                                                     error = e;
                                                     dispatch_semaphore_signal(sem);
                                                 }];
    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC)) != 0) {
        if (outErr) *outErr = [NSError errorWithDomain:@"ptp" code:1 userInfo:@{NSLocalizedDescriptionKey : @"Timed out listing screen content"}];
        return nil;
    }
    if (outErr) *outErr = error;
    return result;
}

static double display_scale(CGDirectDisplayID did) {
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(did);
    if (!mode) return 1.0;
    double pw = (double)CGDisplayModeGetPixelWidth(mode);
    double w = (double)CGDisplayModeGetWidth(mode);
    CGDisplayModeRelease(mode);
    return (w > 0 && pw > 0) ? pw / w : 1.0;
}

static BOOL is_listable_window(SCWindow *w) {
    if (w.windowLayer != 0) return NO;
    if (w.frame.size.width < 120 || w.frame.size.height < 80) return NO;
    if (!w.owningApplication) return NO;
    if (w.owningApplication.processID == getpid()) return NO;
    NSString *bid = w.owningApplication.bundleIdentifier ?: @"";
    if ([bid isEqualToString:@"com.apple.dock"] || [bid isEqualToString:@"com.apple.WindowManager"] ||
        [bid isEqualToString:@"com.apple.controlcenter"])
        return NO;
    return YES;
}

static SCDisplay *find_display(SCShareableContent *c, uint32_t did) {
    for (SCDisplay *d in c.displays)
        if (d.displayID == did) return d;
    return nil;
}

static SCWindow *find_window(SCShareableContent *c, uint32_t wid) {
    for (SCWindow *w in c.windows)
        if (w.windowID == wid) return w;
    return nil;
}

static SCDisplay *main_display(SCShareableContent *c) {
    SCDisplay *d = find_display(c, CGMainDisplayID());
    return d ?: c.displays.firstObject;
}

static SCRunningApplication *find_app(SCShareableContent *c, NSString *bundle) {
    for (SCRunningApplication *a in c.applications)
        if ([a.bundleIdentifier isEqualToString:bundle]) return a;
    return nil;
}

// ───────────────────────────── listing ─────────────────────────────

char *ptp_list_sources_json(void) {
    @autoreleasepool {
        NSError *err = nil;
        SCShareableContent *c = fetch_content(&err);
        if (!c) {
            // Report the real reason instead of a bare failure.
            NSDictionary *e = @{
                @"error" : err.localizedDescription ?: @"no content returned",
                @"domain" : err.domain ?: @"",
                @"code" : @(err.code),
                @"preflight" : CGPreflightScreenCaptureAccess() ? @YES : @NO,
                @"bundle" : NSBundle.mainBundle.bundleIdentifier ?: @"(none)",
            };
            NSData *json = [NSJSONSerialization dataWithJSONObject:e options:0 error:nil];
            char *out = malloc(json.length + 1);
            memcpy(out, json.bytes, json.length);
            out[json.length] = 0;
            return out;
        }

        NSMutableArray *displays = [NSMutableArray array];
        int idx = 1;
        for (SCDisplay *d in c.displays) {
            double s = display_scale(d.displayID);
            [displays addObject:@{
                @"id" : @(d.displayID),
                @"name" : (d.displayID == CGMainDisplayID()) ? @"Main display" : [NSString stringWithFormat:@"Display %d", idx],
                @"width" : @((int)lround(d.width * s)),
                @"height" : @((int)lround(d.height * s)),
                @"is_main" : (d.displayID == CGMainDisplayID()) ? @YES : @NO,
            }];
            idx++;
        }

        double s = display_scale(CGMainDisplayID());
        NSMutableArray *windows = [NSMutableArray array];
        for (SCWindow *w in c.windows) {
            if (!is_listable_window(w)) continue;
            [windows addObject:@{
                @"id" : @(w.windowID),
                @"title" : w.title ?: @"",
                @"app_name" : w.owningApplication.applicationName ?: @"",
                @"bundle_id" : w.owningApplication.bundleIdentifier ?: @"",
                @"width" : @((int)lround(w.frame.size.width * s)),
                @"height" : @((int)lround(w.frame.size.height * s)),
            }];
        }

        NSMutableDictionary *seen = [NSMutableDictionary dictionary];
        NSMutableArray *apps = [NSMutableArray array];
        for (SCRunningApplication *a in c.applications) {
            NSString *bid = a.bundleIdentifier;
            if (bid.length == 0 || a.applicationName.length == 0 || seen[bid]) continue;
            if (a.processID == getpid()) continue;
            seen[bid] = @YES;
            [apps addObject:@{@"bundle_id" : bid, @"name" : a.applicationName, @"pid" : @(a.processID)}];
        }
        [apps sortUsingComparator:^NSComparisonResult(NSDictionary *a, NSDictionary *b) {
            return [a[@"name"] localizedCaseInsensitiveCompare:b[@"name"]];
        }];

        NSDictionary *root = @{@"displays" : displays, @"windows" : windows, @"apps" : apps};
        NSData *json = [NSJSONSerialization dataWithJSONObject:root options:0 error:nil];
        if (!json) return NULL;
        char *out = malloc(json.length + 1);
        memcpy(out, json.bytes, json.length);
        out[json.length] = 0;
        return out;
    }
}

void ptp_free(void *ptr) { free(ptr); }

int ptp_has_screen_access(void) { return CGPreflightScreenCaptureAccess() ? 1 : 0; }
int ptp_request_screen_access(void) { return CGRequestScreenCaptureAccess() ? 1 : 0; }

int ptp_mic_supported(void) {
    if (@available(macOS 15.0, *)) return 1;
    return 0;
}

int ptp_thumbnail(uint32_t kind, uint32_t sid, uint32_t max_w, uint32_t max_h, uint8_t **out_rgba, uint32_t *out_w,
                  uint32_t *out_h) {
    if (@available(macOS 14.0, *)) {
        @autoreleasepool {
            SCShareableContent *c = fetch_content(NULL);
            if (!c) return -2;
            SCContentFilter *filter = nil;
            double srcW = 0, srcH = 0;
            if (kind == PTP_SOURCE_DISPLAY) {
                SCDisplay *d = find_display(c, sid);
                if (!d) return -3;
                filter = [[SCContentFilter alloc] initWithDisplay:d excludingWindows:@[]];
                srcW = d.width;
                srcH = d.height;
            } else if (kind == PTP_SOURCE_WINDOW) {
                SCWindow *w = find_window(c, sid);
                if (!w) return -3;
                filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:w];
                srcW = w.frame.size.width;
                srcH = w.frame.size.height;
            } else {
                return -1;
            }
            double scale = fmin((double)max_w / srcW, (double)max_h / srcH);
            SCStreamConfiguration *cfg = [SCStreamConfiguration new];
            cfg.width = (size_t)fmax(2, lround(srcW * scale));
            cfg.height = (size_t)fmax(2, lround(srcH * scale));
            cfg.showsCursor = NO;

            dispatch_semaphore_t sem = dispatch_semaphore_create(0);
            __block CGImageRef image = NULL;
            [SCScreenshotManager captureImageWithFilter:filter
                                          configuration:cfg
                                      completionHandler:^(CGImageRef img, NSError *e) { (void)e;
                                          if (img) image = CGImageRetain(img);
                                          dispatch_semaphore_signal(sem);
                                      }];
            if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC)) != 0 || !image) return -4;

            size_t w = CGImageGetWidth(image), h = CGImageGetHeight(image);
            uint8_t *buf = calloc(w * h, 4);
            CGColorSpaceRef cs = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
            CGContextRef ctx = CGBitmapContextCreate(buf, w, h, 8, w * 4, cs, kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
            CGContextDrawImage(ctx, CGRectMake(0, 0, w, h), image);
            CGContextRelease(ctx);
            CGColorSpaceRelease(cs);
            CGImageRelease(image);
            *out_rgba = buf;
            *out_w = (uint32_t)w;
            *out_h = (uint32_t)h;
            return 0;
        }
    }
    return -1;
}

// ───────────────────────────── session ─────────────────────────────

#define LB_SAMPLES 6   // samples (100 ms apart) bars must be stable for
#define LB_RECALL 2    // samples needed to re-apply a remembered crop

@interface PTPSession : NSObject <SCStreamOutput, SCStreamDelegate> {
  @public
    ptp_config _cfg;
    atomic_bool _stopped;
    atomic_bool _forceKey;
    VTCompressionSessionRef _vt;
    CVPixelBufferRef _lastFrame;
    int64_t _lastEncodeUs;
    int64_t _lastPtsUs;
    uint32_t _width, _height;   // capture size
    // Letterbox removal: black bars inside the captured picture are cropped
    // off on the GPU before encoding. The capture itself stays uncropped so
    // the bars can be re-checked (and the crop undone) at any time.
    BOOL _lbOn;
    CGRect _lb;                  // crop in capture pixels
    uint32_t _encW, _encH;       // encoder size while _lbOn
    VTPixelTransferSessionRef _xfer;
    CVPixelBufferPoolRef _lbPool;
    int _lbT[LB_SAMPLES], _lbB[LB_SAMPLES], _lbL[LB_SAMPLES], _lbR[LB_SAMPLES];
    int _lbSpill;                // consecutive samples with picture past the crop edge
    int _lbN, _lbPos;
    int64_t _lbLastUs;
    // Crop only while the shared window (or, for a screen, the window on top)
    // covers a whole display, i.e. is fullscreen.
    BOOL _lbAllowed;
    int64_t _fsCheckUs;
    CGDirectDisplayID _displayId;
    int _lbInset;                // screen sharing: rows above the fullscreen app (notch strip)
    // Last crop applied at this capture size: the same bars coming back
    // (e.g. re-entering fullscreen) are cropped again after 2 samples.
    CGRect _lbMemory;
    uint32_t _lbMemW, _lbMemH;
    // window sources: follow the window's size
    BOOL _isWindow;
    uint32_t _windowId;
    double _scaleFactor;
    uint32_t _candW, _candH;
    CGRect _crop, _candCrop;  // window-local points; CGRectNull = whole window
    double _fullPtsW, _fullPtsH; // window size as ScreenCaptureKit captures it (points, uncropped)
    CGRect _cropBounds;          // window-server bounds when the crop was set
    double _cropExtraTop;
    CGRect _defaultSourceRect;
    double _srcW, _srcH; // captured window size in pixels, from frame metadata
    int64_t _candSinceUs, _lastSizeCheckUs;
    // test pattern state
    CVPixelBufferPoolRef _pool;
    uint64_t _frameNo;
    double _tonePhase;
    int64_t _audioStartUs;
    uint64_t _audioFramesSent;
}
@property(strong) SCStream *videoStream;
@property(strong) SCStreamConfiguration *videoConfig;
@property(strong) SCStream *audioStream;
@property(strong) dispatch_queue_t videoQueue;
@property(strong) dispatch_queue_t audioQueue;
@property(strong) dispatch_source_t timer;
@property(strong) dispatch_source_t audioTimer;
@end

static void vt_output(void *refcon, void *frameRefcon, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sb);
static void fit_size(double srcW, double srcH, uint32_t maxW, uint32_t maxH, uint32_t *w, uint32_t *h);

@implementation PTPSession

- (void)dealloc {
    if (_lastFrame) CVPixelBufferRelease(_lastFrame);
    if (_pool) CVPixelBufferPoolRelease(_pool);
    if (_lbPool) CVPixelBufferPoolRelease(_lbPool);
    if (_xfer) {
        VTPixelTransferSessionInvalidate(_xfer);
        CFRelease(_xfer);
    }
}

- (BOOL)setupEncoderWithError:(NSString **)error {
    NSDictionary *spec = @{(id)kVTVideoEncoderSpecification_EnableLowLatencyRateControl : @YES};
    uint32_t ew = _lbOn ? _encW : _width, eh = _lbOn ? _encH : _height;
    OSStatus s = VTCompressionSessionCreate(NULL, ew, eh, kCMVideoCodecType_H264, (__bridge CFDictionaryRef)spec,
                                            NULL, NULL, vt_output, (__bridge void *)self, &_vt);
    if (s != noErr) {
        s = VTCompressionSessionCreate(NULL, ew, eh, kCMVideoCodecType_H264, NULL, NULL, NULL, vt_output,
                                       (__bridge void *)self, &_vt);
    }
    if (s != noErr) {
        *error = [NSString stringWithFormat:@"VTCompressionSessionCreate failed (%d)", (int)s];
        return NO;
    }
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
    if (VTSessionSetProperty(_vt, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel) != noErr)
        VTSessionSetProperty(_vt, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel);
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
    // Tag the stream as BT.709 (H.264 VUI) so decoders don't guess. Note:
    // Chrome on macOS shows <video> with the BT.709 "video gamma" whatever
    // the transfer tag says (midtones lifted: grey 128 → 139), so the viewer
    // draws frames into a canvas, which honours this tag and is exact.
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2);
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2);
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2);
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_ExpectedFrameRate, (__bridge CFNumberRef) @(_cfg.fps));
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_MaxKeyFrameInterval, (__bridge CFNumberRef) @(_cfg.fps * 3));
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, (__bridge CFNumberRef) @3);
    [self applyBitrate:_cfg.bitrate_kbps];
    VTCompressionSessionPrepareToEncodeFrames(_vt);
    return YES;
}

- (void)applyBitrate:(uint32_t)kbps {
    if (!_vt) return;
    int64_t bps = (int64_t)kbps * 1000;
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_AverageBitRate, (__bridge CFNumberRef) @(bps));
    // Cap bursts at 1.5x average over one second (bytes, seconds).
    NSArray *limits = @[ @(bps * 3 / 2 / 8), @1 ];
    VTSessionSetProperty(_vt, kVTCompressionPropertyKey_DataRateLimits, (__bridge CFArrayRef)limits);
}

// Must be called on videoQueue.
- (void)encode:(CVPixelBufferRef)pb {
    [self encode:pb pts:host_time_us()];
}

// pts: capture time (host clock, µs). It becomes the frame's RTP timestamp,
// so viewers play frames back with the same spacing they were captured with.
- (void)encode:(CVPixelBufferRef)pb pts:(int64_t)capturePts {
    if (atomic_load(&_stopped) || !_vt || !pb) return;
    // Right after a resize, a few frames may still arrive at the old size.
    if (CVPixelBufferGetWidth(pb) != _width || CVPixelBufferGetHeight(pb) != _height) return;
    int64_t now = host_time_us();
    int64_t pts = capturePts > 0 ? capturePts : now;
    if (pts <= _lastPtsUs) pts = _lastPtsUs + 1000;
    _lastPtsUs = pts;
    _lastEncodeUs = now;
    CVPixelBufferRef src = pb, cropped = NULL;
    if (_lbOn) {
        cropped = [self cropLetterbox:pb];
        if (!cropped) return;
        src = cropped;
    }
    NSDictionary *opts = nil;
    if (atomic_exchange(&_forceKey, false)) opts = @{(id)kVTEncodeFrameOptionKey_ForceKeyFrame : @YES};
    VTCompressionSessionEncodeFrame(_vt, src, CMTimeMake(pts, 1000000), kCMTimeInvalid, (__bridge CFDictionaryRef)opts,
                                    NULL, NULL);
    if (cropped) CVPixelBufferRelease(cropped);
}

// Crop _lb out of pb (GPU), scaled to the encoder size. Caller releases.
- (CVPixelBufferRef)cropLetterbox:(CVPixelBufferRef)pb {
    if (!_xfer) {
        if (VTPixelTransferSessionCreate(NULL, &_xfer) != noErr) return NULL;
        VTSessionSetProperty(_xfer, kVTPixelTransferPropertyKey_ScalingMode, kVTScalingMode_CropSourceToCleanAperture);
    }
    CVPixelBufferRef out = NULL;
    if (!_lbPool || CVPixelBufferPoolCreatePixelBuffer(NULL, _lbPool, &out) != kCVReturnSuccess) return NULL;
    double W = CVPixelBufferGetWidth(pb), H = CVPixelBufferGetHeight(pb);
    // Clean aperture offsets are measured from the buffer's centre.
    NSDictionary *ca = @{
        (id)kCVImageBufferCleanApertureWidthKey : @(_lb.size.width),
        (id)kCVImageBufferCleanApertureHeightKey : @(_lb.size.height),
        (id)kCVImageBufferCleanApertureHorizontalOffsetKey : @(CGRectGetMidX(_lb) - W / 2),
        (id)kCVImageBufferCleanApertureVerticalOffsetKey : @(CGRectGetMidY(_lb) - H / 2),
    };
    CFTypeRef prev = CVBufferCopyAttachment(pb, kCVImageBufferCleanApertureKey, NULL);
    CVBufferSetAttachment(pb, kCVImageBufferCleanApertureKey, (__bridge CFDictionaryRef)ca, kCVAttachmentMode_ShouldNotPropagate);
    OSStatus st = VTPixelTransferSessionTransferImage(_xfer, pb, out);
    if (prev) {
        CVBufferSetAttachment(pb, kCVImageBufferCleanApertureKey, prev, kCVAttachmentMode_ShouldNotPropagate);
        CFRelease(prev);
    } else {
        CVBufferRemoveAttachment(pb, kCVImageBufferCleanApertureKey);
    }
    if (st != noErr) {
        CVPixelBufferRelease(out);
        return NULL;
    }
    return out;
}

// Rows/columns of near-black pixels at each edge of the luma plane, sampled
// along three lines (¼, ½, ¾). Returns NO for an (almost) entirely black frame.
static BOOL measure_bars(CVPixelBufferRef pb, int *top, int *bottom, int *left, int *right) {
    if (CVPixelBufferGetPixelFormatType(pb) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange) return NO;
    if (CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) return NO;
    const uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(pb, 0);
    size_t stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
    int W = (int)CVPixelBufferGetWidthOfPlane(pb, 0), H = (int)CVPixelBufferGetHeightOfPlane(pb, 0);
    const int black = 24; // video-range black is 16
    int t = H, b = H, l = W, r = W;
    for (int k = 1; k <= 3; k++) {
        int x = W * k / 4, row = H * k / 4;
        int i = 0;
        while (i < H / 2 && y[(size_t)i * stride + x] <= black) i++;
        if (i < t) t = i;
        i = 0;
        while (i < H / 2 && y[(size_t)(H - 1 - i) * stride + x] <= black) i++;
        if (i < b) b = i;
        i = 0;
        while (i < W / 2 && y[(size_t)row * stride + i] <= black) i++;
        if (i < l) l = i;
        i = 0;
        while (i < W / 2 && y[(size_t)row * stride + (W - 1 - i)] <= black) i++;
        if (i < r) r = i;
    }
    CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    if (t >= H / 2 || l >= W / 2) return NO; // black frame: no information
    *top = t; *bottom = b; *left = l; *right = r;
    return YES;
}

static void bar_stats(const int *v, int n, int *mn, int *mx) {
    *mn = INT32_MAX; *mx = 0;
    for (int i = 0; i < n; i++) {
        if (v[i] < *mn) *mn = v[i];
        if (v[i] > *mx) *mx = v[i];
    }
}

// Bars on one axis count only if both sides are similar (a centred picture),
// at least 2% of the frame, and unchanged across all samples given, so dark
// scenes are not mistaken for bars.
static int stable_bar(const int *a, const int *b, int n, int extent) {
    int amn, amx, bmn, bmx;
    bar_stats(a, n, &amn, &amx);
    bar_stats(b, n, &bmn, &bmx);
    int m = amn < bmn ? amn : bmn;
    if (m < extent / 50) return 0;
    if (amx - amn > 4 || bmx - bmn > 4) return 0;
    if (abs(amn - bmn) > (extent / 100 > 4 ? extent / 100 : 4)) return 0;
    // +2: the first row/column past the bar is often a dark blend of the edge.
    return (m + 2) & ~1;
}

// Fraction of 16 points along a line (row y, or column x) that are not black.
static double line_fill(CVPixelBufferRef pb, BOOL row, int at, int from, int to) {
    if (CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) return 0;
    const uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(pb, 0);
    size_t stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
    int lit = 0;
    for (int k = 0; k < 16; k++) {
        int p = from + (to - from) * (2 * k + 1) / 32;
        uint8_t v = row ? y[(size_t)at * stride + p] : y[(size_t)p * stride + at];
        if (v > 24) lit++;
    }
    CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    return lit / 16.0;
}

// Height of the area above a fullscreen app (the camera notch on MacBooks), points.
static double notch_inset(CGDirectDisplayID did) {
    if (@available(macOS 12.0, *)) {
        for (NSScreen *scr in NSScreen.screens) {
            NSNumber *n = scr.deviceDescription[@"NSScreenNumber"];
            if (n && n.unsignedIntValue == did) return scr.safeAreaInsets.top;
        }
    }
    return 0;
}

// Whether window bounds b fill display d the way a fullscreen app does: the
// whole display, or all of it below the notch. Sets *top to the gap above.
static BOOL fills_display(CGRect b, CGDirectDisplayID did, double *top) {
    CGRect d = CGDisplayBounds(did);
    double gap = b.origin.y - d.origin.y;
    if (gap > 2 && gap > notch_inset(did) + 2) return NO;
    if (b.origin.x > d.origin.x + 2 || CGRectGetMaxX(b) < CGRectGetMaxX(d) - 2 || CGRectGetMaxY(b) < CGRectGetMaxY(d) - 2) return NO;
    if (top) *top = gap > 2 ? gap : 0;
    return YES;
}

static BOOL covers_display(CGRect b, CGDirectDisplayID *which) {
    CGDirectDisplayID ids[16];
    uint32_t count = 0;
    CGGetActiveDisplayList(16, ids, &count);
    for (uint32_t i = 0; i < count; i++) {
        if (fills_display(b, ids[i], NULL)) {
            if (which) *which = ids[i];
            return YES;
        }
    }
    return NO;
}

// Is the shared window fullscreen, or (sharing a screen) the topmost normal
// window on that screen? Black bars are cropped only then. On videoQueue.
- (void)checkFullscreen {
    int64_t now = host_time_us();
    if (now - _fsCheckUs < 200000) return;
    _fsCheckUs = now;
    BOOL fs = NO;
    int inset = 0;
    if (_isWindow) {
        CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, _windowId);
        if (list) {
            CGRect b;
            if (CFArrayGetCount(list) > 0) {
                CFDictionaryRef bd = CFDictionaryGetValue(CFArrayGetValueAtIndex(list, 0), kCGWindowBounds);
                fs = bd && CGRectMakeWithDictionaryRepresentation(bd, &b) && covers_display(b, NULL);
            }
            CFRelease(list);
        }
    } else if (_displayId) {
        CGRect d = CGDisplayBounds(_displayId);
        CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID);
        if (list) {
            // Front to back: the first sizeable normal window on this display decides.
            for (CFIndex i = 0; i < CFArrayGetCount(list); i++) {
                CFDictionaryRef w = CFArrayGetValueAtIndex(list, i);
                int layer = -1;
                CFNumberRef ln = CFDictionaryGetValue(w, kCGWindowLayer);
                if (!ln || !CFNumberGetValue(ln, kCFNumberIntType, &layer) || layer != 0) continue;
                CGRect b;
                CFDictionaryRef bd = CFDictionaryGetValue(w, kCGWindowBounds);
                if (!bd || !CGRectMakeWithDictionaryRepresentation(bd, &b)) continue;
                CGRect inter = CGRectIntersection(b, d);
                if (CGRectIsNull(inter)) continue;
                // Skip helper windows (tooltips, Chrome's status bubble...):
                // invisible, or under 10% of the screen.
                double alpha = 1;
                CFNumberRef an = CFDictionaryGetValue(w, kCGWindowAlpha);
                if (an) CFNumberGetValue(an, kCFNumberDoubleType, &alpha);
                if (alpha < 0.1 || inter.size.width * inter.size.height < d.size.width * d.size.height * 0.1) continue;
                double top = 0;
                fs = fills_display(b, _displayId, &top);
                // The strip above the app is part of the shared screen: rows
                // in capture pixels, left out when measuring the bars.
                if (fs && top > 0) inset = (int)ceil(top * _height / d.size.height);
                if (getenv("PTP_DEBUG_FS")) {
                    NSString *owner = (__bridge NSString *)CFDictionaryGetValue(w, kCGWindowOwnerName);
                    fprintf(stderr, "dbg fs top window %s %.0f,%.0f %.0fx%.0f display %.0f,%.0f %.0fx%.0f\n", owner.UTF8String ?: "?",
                            b.origin.x, b.origin.y, b.size.width, b.size.height, d.origin.x, d.origin.y, d.size.width, d.size.height);
                }
                break;
            }
            CFRelease(list);
        }
    }
    if (fs == _lbAllowed && inset == _lbInset) return;
    _lbAllowed = fs;
    _lbInset = inset;
    _lbN = _lbPos = 0;
    if (getenv("PTP_DEBUG")) fprintf(stderr, "dbg fullscreen %s\n", fs ? "yes" : "no");
    if (!fs && _lbOn) [self setLetterbox:CGRectNull];
}

// Crop rect for bars of v rows (top and bottom) and h columns (each side),
// below `inset` rows at the top that are cut off too.
static CGRect lb_rect(int v, int h, int W, int H, int inset) {
    int top = (v + inset + 1) & ~1;
    return CGRectMake(h, top, (W - 2 * h) & ~1, (H - top - v) & ~1);
}

// The last k samples of a ring, oldest first.
static void last_samples(const int *ring, int pos, int k, int *out) {
    for (int i = 0; i < k; i++) out[i] = ring[(pos - k + i + LB_SAMPLES) % LB_SAMPLES];
}

// Called on videoQueue for captured frames and, from the timer, for the last
// frame (a paused picture sends no new frames); samples 10×/s. Bars must be
// stable for 0.6 s, or 0.2 s if they match the last crop at this size.
- (void)checkLetterbox:(CVPixelBufferRef)pb {
    if (!_lbAllowed) return;
    int64_t now = host_time_us();
    if (now - _lbLastUs < 100000) return;
    _lbLastUs = now;
    int W = (int)CVPixelBufferGetWidth(pb), H = (int)CVPixelBufferGetHeight(pb);

    if (_lbOn) {
        // Undo only when the picture itself grows into a bar: check a line just
        // outside each cropped edge, across the whole width/height. Player
        // controls or subtitles drawn further out in the bar don't count, so
        // moving the mouse over a fullscreen video doesn't make the crop flap.
        int x0 = (int)_lb.origin.x, y0 = (int)_lb.origin.y;
        int x1 = (int)CGRectGetMaxX(_lb), y1 = (int)CGRectGetMaxY(_lb);
        BOOL spill = NO;
        if (y0 >= 8)
            spill |= line_fill(pb, YES, y0 - 6, x0, x1) >= 0.5 || line_fill(pb, YES, y1 + 5, x0, x1) >= 0.5;
        if (x0 >= 8)
            spill |= line_fill(pb, NO, x0 - 6, y0, y1) >= 0.5 || line_fill(pb, NO, x1 + 5, y0, y1) >= 0.5;
        _lbSpill = spill ? _lbSpill + 1 : 0;
        if (_lbSpill >= 3) {
            _lbN = _lbPos = 0;
            _lbSpill = 0;
            [self setLetterbox:CGRectNull];
        }
        return;
    }
    int t, b, l, r;
    if (!measure_bars(pb, &t, &b, &l, &r)) return;
    t = t > _lbInset ? t - _lbInset : 0;
    _lbT[_lbPos] = t; _lbB[_lbPos] = b; _lbL[_lbPos] = l; _lbR[_lbPos] = r;
    _lbPos = (_lbPos + 1) % LB_SAMPLES;
    if (_lbN < LB_SAMPLES) _lbN++;

    int k = _lbN;
    BOOL remembered = !CGRectIsNull(_lbMemory) && _lbMemW == (uint32_t)W && _lbMemH == (uint32_t)H;
    if (k < (remembered ? LB_RECALL : LB_SAMPLES)) return;
    if (k > LB_SAMPLES) k = LB_SAMPLES;
    int st[LB_SAMPLES], sb[LB_SAMPLES], sl[LB_SAMPLES], sr[LB_SAMPLES];
    if (remembered) {
        last_samples(_lbT, _lbPos, LB_RECALL, st);
        last_samples(_lbB, _lbPos, LB_RECALL, sb);
        last_samples(_lbL, _lbPos, LB_RECALL, sl);
        last_samples(_lbR, _lbPos, LB_RECALL, sr);
        int v = stable_bar(st, sb, LB_RECALL, H - _lbInset), h = stable_bar(sl, sr, LB_RECALL, W);
        if ((v || h) && CGRectEqualToRect(lb_rect(v, h, W, H, _lbInset), _lbMemory)) {
            [self setLetterbox:_lbMemory];
            return;
        }
        if (k < LB_SAMPLES) return;
    }
    last_samples(_lbT, _lbPos, LB_SAMPLES, st);
    last_samples(_lbB, _lbPos, LB_SAMPLES, sb);
    last_samples(_lbL, _lbPos, LB_SAMPLES, sl);
    last_samples(_lbR, _lbPos, LB_SAMPLES, sr);
    int v = stable_bar(st, sb, LB_SAMPLES, H - _lbInset);
    int h = stable_bar(sl, sr, LB_SAMPLES, W);
    if (!v && !h) return;
    CGRect crop = lb_rect(v, h, W, H, _lbInset);
    _lbMemory = crop;
    _lbMemW = W;
    _lbMemH = H;
    [self setLetterbox:crop];
}

- (void)setLetterbox:(CGRect)crop {
    BOOL on = !CGRectIsNull(crop);
    if (!on && !_lbOn) return;
    if (_vt) {
        VTCompressionSessionCompleteFrames(_vt, kCMTimeInvalid);
        VTCompressionSessionInvalidate(_vt);
        CFRelease(_vt);
        _vt = NULL;
    }
    _lbOn = on;
    _lb = crop;
    _lbSpill = 0;
    if (_lbPool) {
        CVPixelBufferPoolRelease(_lbPool);
        _lbPool = NULL;
    }
    if (on) {
        fit_size(crop.size.width, crop.size.height, _cfg.max_width, _cfg.max_height, &_encW, &_encH);
        NSDictionary *attrs = @{
            (id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
            (id)kCVPixelBufferWidthKey : @(_encW),
            (id)kCVPixelBufferHeightKey : @(_encH),
            (id)kCVPixelBufferIOSurfacePropertiesKey : @{},
        };
        CVPixelBufferPoolCreate(NULL, NULL, (__bridge CFDictionaryRef)attrs, &_lbPool);
    }
    NSString *e = nil;
    if (![self setupEncoderWithError:&e]) {
        _lbOn = NO;
        [self setupEncoderWithError:&e];
    }
    atomic_store(&_forceKey, true);
    if (getenv("PTP_DEBUG"))
        fprintf(stderr, "dbg letterbox %s %.0f,%.0f %.0fx%.0f -> %ux%u\n", _lbOn ? "on" : "off", crop.origin.x,
                crop.origin.y, crop.size.width, crop.size.height, _lbOn ? _encW : _width, _lbOn ? _encH : _height);
}

- (void)rememberFrame:(CVPixelBufferRef)pb {
    if (_lastFrame == pb) return;
    if (_lastFrame) CVPixelBufferRelease(_lastFrame);
    _lastFrame = pb ? CVPixelBufferRetain(pb) : NULL;
}

// Static content produces no new frames from ScreenCaptureKit. Re-encode the
// last frame so new viewers / keyframe requests still get a picture.
- (void)tick {
    if (atomic_load(&_stopped)) return;
    if (_isWindow) [self checkWindowSize];
    [self checkFullscreen];
    if (!_lastFrame) return;
    if (CVPixelBufferGetWidth(_lastFrame) == _width && CVPixelBufferGetHeight(_lastFrame) == _height)
        [self checkLetterbox:_lastFrame];
    int64_t idle = host_time_us() - _lastEncodeUs;
    if (idle > 500000 || (atomic_load(&_forceKey) && idle > 40000)) [self encode:_lastFrame];
}

static void fit_size(double srcW, double srcH, uint32_t maxW, uint32_t maxH, uint32_t *w, uint32_t *h);

static double backing_scale(CGDirectDisplayID did) {
    for (NSScreen *scr in NSScreen.screens) {
        NSNumber *n = scr.deviceDescription[@"NSScreenNumber"];
        if (n && n.unsignedIntValue == did) return scr.backingScaleFactor;
    }
    return display_scale(did);
}

// Window sources: the stream size is fixed at start, so when the window is
// resized ScreenCaptureKit shrinks it inside the old frame (black borders).
// Follow the window instead: once a new geometry has been stable for 300 ms,
// resize the capture and restart the encoder (viewers get a new keyframe).
//
// Only the part of the window on the display it mostly sits on is streamed.
// Fullscreen Chrome, for example, keeps its hidden toolbar ~120pt above the
// top of the screen; capturing the whole window shows that as a black band.
// Must be called on videoQueue.
- (void)checkWindowSize {
    int64_t now = host_time_us();
    if (now - _lastSizeCheckUs < 100000) return;
    _lastSizeCheckUs = now;

    CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, _windowId);
    if (!list) return;
    CGRect b = CGRectZero;
    BOOL ok = NO;
    if (CFArrayGetCount(list) > 0) {
        CFDictionaryRef bd = CFDictionaryGetValue(CFArrayGetValueAtIndex(list, 0), kCGWindowBounds);
        ok = bd && CGRectMakeWithDictionaryRepresentation(bd, &b);
    }
    CFRelease(list);
    if (!ok || b.size.width < 8 || b.size.height < 8) return;

    // Display holding the largest part of the window.
    CGDirectDisplayID ids[16];
    uint32_t count = 0;
    CGGetActiveDisplayList(16, ids, &count);
    CGRect vis = b;
    double bestArea = 0, sc = _scaleFactor > 0 ? _scaleFactor : 1.0;
    for (uint32_t i = 0; i < count; i++) {
        CGRect inter = CGRectIntersection(b, CGDisplayBounds(ids[i]));
        double area = CGRectIsNull(inter) ? 0 : inter.size.width * inter.size.height;
        if (area > bestArea) {
            bestArea = area;
            vis = inter;
            sc = backing_scale(ids[i]);
        }
    }
    vis = CGRectIntegral(vis);
    if (vis.size.width < 8 || vis.size.height < 8) return;
    b = CGRectIntegral(b);

    // ScreenCaptureKit can capture more than the window server reports: a
    // fullscreen Chrome window is ~124pt taller than its bounds, with the extra
    // (its hidden toolbar, rendered black) at the top. A fullscreen window on a
    // display left of the main one (negative x origin) is also padded on the
    // right by that offset (5120pt wide captured as 6661pt). Measure both from
    // the uncropped frames and crop them off with any off-screen part.
    double extraTop;
    BOOL extraRight = NO;
    if (!CGRectIsNull(_crop)) {
        if (!CGRectEqualToRect(b, _cropBounds)) {
            // The window changed while cropped. Frames only show the cropped
            // area now, so drop the crop and re-measure on the next pass.
            vis = b;
            extraTop = -1;
        } else {
            _candW = _candH = 0;
            return; // same window, crop still valid
        }
    } else {
        // Captured smaller than the window: frames haven't caught up yet.
        if (_fullPtsW < b.size.width - 2 || _fullPtsH < b.size.height - 2) return;
        extraTop = _fullPtsH - b.size.height;
        if (extraTop < 2) extraTop = 0;
        extraRight = _fullPtsW > b.size.width + 2;
    }
    CGRect crop = CGRectNull;
    if (extraTop > 0 || extraRight || (extraTop == 0 && !CGRectEqualToRect(vis, b)))
        crop = CGRectMake(vis.origin.x - b.origin.x, vis.origin.y - b.origin.y + extraTop, vis.size.width, vis.size.height);

    uint32_t nw, nh;
    fit_size(vis.size.width * sc, vis.size.height * sc, _cfg.max_width, _cfg.max_height, &nw, &nh);
    if (nw == _width && nh == _height && CGRectEqualToRect(crop, _crop)) {
        _candW = _candH = 0;
        return;
    }
    if (nw != _candW || nh != _candH || !CGRectEqualToRect(crop, _candCrop)) {
        _candW = nw;
        _candH = nh;
        _candCrop = crop;
        _candSinceUs = now;
        return;
    }
    if (now - _candSinceUs < 300000) return;
    _candW = _candH = 0;
    _cropBounds = b;
    _cropExtraTop = extraTop > 0 ? extraTop : 0;
    if (getenv("PTP_DEBUG"))
        fprintf(stderr, "dbg window bounds %.0f,%.0f %.0fx%.0f captured %.0fx%.0f\n", b.origin.x, b.origin.y, b.size.width,
                b.size.height, _fullPtsW, _fullPtsH);
    [self resizeTo:nw height:nh crop:crop];
}

- (void)resizeTo:(uint32_t)w height:(uint32_t)h crop:(CGRect)crop {
    uint32_t oldW = _width, oldH = _height;
    if (_vt) {
        VTCompressionSessionCompleteFrames(_vt, kCMTimeInvalid);
        VTCompressionSessionInvalidate(_vt);
        CFRelease(_vt);
        _vt = NULL;
    }
    _width = w;
    _height = h;
    _lbOn = NO;
    _lbN = _lbPos = 0;
    NSString *e = nil;
    if (![self setupEncoderWithError:&e]) {
        // Fall back to the old size rather than stopping the stream.
        _width = oldW;
        _height = oldH;
        [self setupEncoderWithError:&e];
        return;
    }
    _crop = crop;
    atomic_store(&_forceKey, true);
    [self rememberFrame:NULL];
    SCStreamConfiguration *c = self.videoConfig;
    c.width = w;
    c.height = h;
    c.sourceRect = CGRectIsNull(crop) ? _defaultSourceRect : crop;
    if (getenv("PTP_DEBUG"))
        fprintf(stderr, "dbg resize %ux%u crop %.0f,%.0f %.0fx%.0f\n", w, h, crop.origin.x, crop.origin.y, crop.size.width, crop.size.height);
    [self.videoStream updateConfiguration:c completionHandler:^(NSError *err) {
        if (err) NSLog(@"ptp: resize to %ux%u failed: %@", w, h, err);
    }];
}

- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sb ofType:(SCStreamOutputType)type {
    if (atomic_load(&_stopped) || !CMSampleBufferIsValid(sb)) return;
    if (type == SCStreamOutputTypeScreen) {
        if (stream != self.videoStream) return; // the audio stream's dummy video
        CFArrayRef atts = CMSampleBufferGetSampleAttachmentsArray(sb, NO);
        if (!atts || CFArrayGetCount(atts) == 0) return;
        NSDictionary *a = (__bridge NSDictionary *)CFArrayGetValueAtIndex(atts, 0);
        NSNumber *status = a[SCStreamFrameInfoStatus];
        if (!status || status.integerValue != SCFrameStatusComplete) return;
        NSNumber *scale = a[SCStreamFrameInfoScaleFactor];
        if (scale.doubleValue > 0) _scaleFactor = scale.doubleValue;
        if (_isWindow) {
            // contentRect (points, inside the output) / contentScale = window size
            // in points; × scaleFactor = window size in pixels.
            CGRect cr = CGRectZero;
            CFDictionaryRef crd = (__bridge CFDictionaryRef)a[SCStreamFrameInfoContentRect];
            double cs = [a[SCStreamFrameInfoContentScale] doubleValue];
            if (crd && CGRectMakeWithDictionaryRepresentation(crd, &cr) && cs > 0 && _scaleFactor > 0) {
                _srcW = cr.size.width / cs * _scaleFactor;
                _srcH = cr.size.height / cs * _scaleFactor;
                // contentRect/contentScale give the captured window size even while
                // the output is still at an old size, so a new window size and its
                // crop can be applied in one step instead of two.
                if (CGRectIsNull(_crop)) {
                    _fullPtsW = cr.size.width / cs;
                    _fullPtsH = cr.size.height / cs;
                }
            }
        }
        CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(sb);
        if (!pb) return;
        static int dbg = -1;
        if (dbg < 0) dbg = getenv("PTP_DEBUG") != NULL;
        static uint64_t n = 0;
        if (dbg && (n++ % 60) == 0) {
            CGRect cr = CGRectZero;
            CFDictionaryRef crd = (__bridge CFDictionaryRef)a[SCStreamFrameInfoContentRect];
            if (crd) CGRectMakeWithDictionaryRepresentation(crd, &cr);
            fprintf(stderr, "dbg buf %zux%zu cfg %ux%u contentRect %.0f,%.0f %.0fx%.0f contentScale %.3f scaleFactor %.2f window px %.0fx%.0f\n",
                    CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb), _width, _height, cr.origin.x, cr.origin.y,
                    cr.size.width, cr.size.height, [a[SCStreamFrameInfoContentScale] doubleValue], scale.doubleValue,
                    _srcW, _srcH);
        }
        if (CVPixelBufferGetWidth(pb) == _width && CVPixelBufferGetHeight(pb) == _height) [self checkLetterbox:pb];
        [self rememberFrame:pb];
        CMTime cpts = CMSampleBufferGetPresentationTimeStamp(sb);
        [self encode:pb pts:CMTIME_IS_VALID(cpts) ? (int64_t)(CMTimeGetSeconds(cpts) * 1e6) : 0];
        return;
    }
    int isMic = 0;
    if (type == SCStreamOutputTypeAudio) {
        isMic = 0;
    } else {
        if (@available(macOS 15.0, *)) {
            if (type == SCStreamOutputTypeMicrophone) isMic = 1;
            else return;
        } else {
            return;
        }
    }
    [self deliverAudio:sb isMic:isMic];
}

- (void)deliverAudio:(CMSampleBufferRef)sb isMic:(int)isMic {
    CMFormatDescriptionRef fmt = CMSampleBufferGetFormatDescription(sb);
    const AudioStreamBasicDescription *asbd = fmt ? CMAudioFormatDescriptionGetStreamBasicDescription(fmt) : NULL;
    if (!asbd) return;
    CMItemCount frames = CMSampleBufferGetNumSamples(sb);
    if (frames <= 0) return;

    size_t ablSize = 0;
    CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, &ablSize, NULL, 0, NULL, NULL, 0, NULL);
    if (ablSize == 0) return;
    AudioBufferList *abl = malloc(ablSize);
    CMBlockBufferRef block = NULL;
    OSStatus s = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sb, NULL, abl, ablSize, NULL, NULL, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, &block);
    if (s != noErr) {
        free(abl);
        return;
    }

    BOOL isFloat = (asbd->mFormatFlags & kAudioFormatFlagIsFloat) != 0;
    BOOL nonInterleaved = (asbd->mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;
    uint32_t ch = asbd->mChannelsPerFrame ?: 1;
    uint32_t bits = asbd->mBitsPerChannel;
    float *out = malloc(sizeof(float) * frames * 2);

    for (CMItemCount i = 0; i < frames; i++) {
        float l = 0, r = 0;
        for (uint32_t c = 0; c < 2; c++) {
            uint32_t srcC = (c < ch) ? c : 0;
            float v = 0;
            if (nonInterleaved) {
                if (srcC >= abl->mNumberBuffers) continue;
                void *data = abl->mBuffers[srcC].mData;
                if (isFloat && bits == 32) v = ((float *)data)[i];
                else if (!isFloat && bits == 16) v = ((int16_t *)data)[i] / 32768.0f;
                else if (!isFloat && bits == 32) v = ((int32_t *)data)[i] / 2147483648.0f;
            } else {
                void *data = abl->mBuffers[0].mData;
                size_t at = (size_t)i * ch + srcC;
                if (isFloat && bits == 32) v = ((float *)data)[at];
                else if (!isFloat && bits == 16) v = ((int16_t *)data)[at] / 32768.0f;
                else if (!isFloat && bits == 32) v = ((int32_t *)data)[at] / 2147483648.0f;
            }
            if (c == 0) l = v;
            else r = v;
        }
        out[i * 2] = l;
        out[i * 2 + 1] = r;
    }
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sb);
    int64_t ptsUs = CMTIME_IS_VALID(pts) ? (int64_t)(CMTimeGetSeconds(pts) * 1e6) : host_time_us();
    if (!atomic_load(&_stopped) && _cfg.on_audio)
        _cfg.on_audio(_cfg.ctx, out, (size_t)frames, (uint32_t)asbd->mSampleRate, ptsUs, isMic);
    free(out);
    free(abl);
    if (block) CFRelease(block);
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    if (atomic_load(&_stopped)) return;
    NSString *msg = [NSString stringWithFormat:@"Capture stopped: %@", error.localizedDescription ?: @"unknown"];
    if (_cfg.on_error) _cfg.on_error(_cfg.ctx, msg.UTF8String);
}

// ───── test pattern (no permissions needed; used for testing the pipeline) ─────

- (void)testPatternFrame {
    if (atomic_load(&_stopped)) return;
    CVPixelBufferRef pb = NULL;
    if (CVPixelBufferPoolCreatePixelBuffer(NULL, _pool, &pb) != kCVReturnSuccess) return;
    CVPixelBufferLockBaseAddress(pb, 0);
    uint8_t *base = CVPixelBufferGetBaseAddress(pb);
    size_t stride = CVPixelBufferGetBytesPerRow(pb);
    uint32_t w = _width, h = _height;
    uint64_t f = _frameNo++;
    static const uint32_t bars[8] = {0xFFFFFF, 0xFFFF00, 0x00FFFF, 0x00FF00, 0xFF00FF, 0xFF0000, 0x0000FF, 0x202020};
    uint32_t boxX = (uint32_t)((f * 6) % (w > 160 ? w - 160 : 1));
    uint32_t boxY = h / 2 - 80;
    for (uint32_t y = 0; y < h; y++) {
        uint32_t *row = (uint32_t *)(base + y * stride);
        for (uint32_t x = 0; x < w; x++) {
            uint32_t rgb;
            if (x >= boxX && x < boxX + 160 && y >= boxY && y < boxY + 160) rgb = 0x101010;
            else if (y > h * 3 / 4) rgb = (uint32_t)(((x + f * 4) % w) * 255 / w) * 0x010101;
            else rgb = bars[x * 8 / w];
            // BGRA little-endian: 0xAARRGGBB
            row[x] = 0xFF000000 | rgb;
        }
    }
    CVPixelBufferUnlockBaseAddress(pb, 0);
    [self rememberFrame:pb];
    [self encode:pb];
    CVPixelBufferRelease(pb);
}

- (void)testPatternAudio {
    if (atomic_load(&_stopped) || !_cfg.on_audio) return;
    // Emit audio in real time: 440 Hz beep for 200ms each second.
    int64_t now = host_time_us();
    if (_audioStartUs == 0) _audioStartUs = now;
    uint64_t due = (uint64_t)((now - _audioStartUs) * 48 / 1000);
    if (due <= _audioFramesSent) return;
    size_t n = (size_t)(due - _audioFramesSent);
    float *buf = malloc(sizeof(float) * n * 2);
    for (size_t i = 0; i < n; i++) {
        uint64_t idx = _audioFramesSent + i;
        BOOL on = (idx % 48000) < 9600;
        float v = on ? 0.2f * sinf((float)_tonePhase) : 0;
        _tonePhase += 2 * M_PI * 440.0 / 48000.0;
        if (_tonePhase > 2 * M_PI) _tonePhase -= 2 * M_PI;
        buf[i * 2] = v;
        buf[i * 2 + 1] = v;
    }
    _cfg.on_audio(_cfg.ctx, buf, n, 48000, now, 0);
    _audioFramesSent += n;
    free(buf);
}

@end

static void vt_output(void *refcon, void *frameRefcon, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sb) {
    (void)frameRefcon; (void)flags;
    PTPSession *self = (__bridge PTPSession *)refcon;
    if (status != noErr || !sb || atomic_load(&self->_stopped) || !self->_cfg.on_video) return;
    if (!CMSampleBufferDataIsReady(sb)) return;

    BOOL key = YES;
    CFArrayRef atts = CMSampleBufferGetSampleAttachmentsArray(sb, false);
    if (atts && CFArrayGetCount(atts) > 0) {
        CFDictionaryRef d = CFArrayGetValueAtIndex(atts, 0);
        CFBooleanRef notSync = NULL;
        if (CFDictionaryGetValueIfPresent(d, kCMSampleAttachmentKey_NotSync, (const void **)&notSync) && notSync)
            key = !CFBooleanGetValue(notSync);
    }

    static const uint8_t startCode[4] = {0, 0, 0, 1};
    NSMutableData *out = [NSMutableData data];
    CMFormatDescriptionRef fmt = CMSampleBufferGetFormatDescription(sb);
    int nalLen = 4;
    if (fmt) {
        size_t count = 0;
        int hdr = 4;
        if (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, 0, NULL, NULL, &count, &hdr) == noErr) nalLen = hdr;
        if (key) {
            for (size_t i = 0; i < count; i++) {
                const uint8_t *ps = NULL;
                size_t psLen = 0;
                if (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, i, &ps, &psLen, NULL, NULL) == noErr) {
                    static int dumped = 0;
                    if (i == 0 && !dumped && getenv("PTP_DEBUG")) {
                        dumped = 1;
                        fprintf(stderr, "dbg sps ");
                        for (size_t k = 0; k < psLen; k++) fprintf(stderr, "%02x", ps[k]);
                        fprintf(stderr, "\n");
                    }
                    [out appendBytes:startCode length:4];
                    [out appendBytes:ps length:psLen];
                }
            }
        }
    }

    CMBlockBufferRef bb = CMSampleBufferGetDataBuffer(sb);
    size_t total = CMBlockBufferGetDataLength(bb);
    uint8_t *data = malloc(total);
    if (CMBlockBufferCopyDataBytes(bb, 0, total, data) != kCMBlockBufferNoErr) {
        free(data);
        return;
    }
    size_t off = 0;
    while (off + nalLen <= total) {
        uint32_t len = 0;
        for (int i = 0; i < nalLen; i++) len = (len << 8) | data[off + i];
        off += nalLen;
        if (len == 0 || off + len > total) break;
        [out appendBytes:startCode length:4];
        [out appendBytes:data + off length:len];
        off += len;
    }
    free(data);

    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sb);
    int64_t ptsUs = (int64_t)(CMTimeGetSeconds(pts) * 1e6);
    self->_cfg.on_video(self->_cfg.ctx, out.bytes, out.length, ptsUs, key ? 1 : 0);
}

static void fit_size(double srcW, double srcH, uint32_t maxW, uint32_t maxH, uint32_t *w, uint32_t *h) {
    double scale = fmin(1.0, fmin((double)maxW / srcW, (double)maxH / srcH));
    uint32_t ow = (uint32_t)lround(srcW * scale) & ~1u;
    uint32_t oh = (uint32_t)lround(srcH * scale) & ~1u;
    *w = ow < 16 ? 16 : ow;
    *h = oh < 16 ? 16 : oh;
}

static BOOL start_stream(SCStream *stream, NSString **error) {
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block NSError *e = nil;
    [stream startCaptureWithCompletionHandler:^(NSError *err) {
        e = err;
        dispatch_semaphore_signal(sem);
    }];
    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC)) != 0) {
        *error = @"Timed out starting capture";
        return NO;
    }
    if (e) {
        *error = e.localizedDescription;
        return NO;
    }
    return YES;
}

static void stop_stream(SCStream *stream) {
    if (!stream) return;
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    [stream stopCaptureWithCompletionHandler:^(NSError *err) { (void)err;
        dispatch_semaphore_signal(sem);
    }];
    dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC));
}

void *ptp_start(const ptp_config *cfg, uint32_t *out_width, uint32_t *out_height, char *err, size_t err_len) {
    @autoreleasepool {
        PTPSession *s = [PTPSession new];
        s->_cfg = *cfg;
        // Strings are only valid during this call.
        s->_cfg.audio_app_bundle = NULL;
        s->_cfg.excluded_bundles = NULL;
        atomic_store(&s->_stopped, false);
        atomic_store(&s->_forceKey, true);
        s.videoQueue = dispatch_queue_create("ptp.video", DISPATCH_QUEUE_SERIAL);
        s.audioQueue = dispatch_queue_create("ptp.audio", DISPATCH_QUEUE_SERIAL);
        uint32_t fps = cfg->fps ?: 60;
        NSString *e = nil;

        if (cfg->source_kind == PTP_SOURCE_TEST_PATTERN) {
            fit_size(1920, 1080, cfg->max_width, cfg->max_height, &s->_width, &s->_height);
            NSDictionary *attrs = @{
                (id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_32BGRA),
                (id)kCVPixelBufferWidthKey : @(s->_width),
                (id)kCVPixelBufferHeightKey : @(s->_height),
                (id)kCVPixelBufferIOSurfacePropertiesKey : @{},
            };
            CVPixelBufferPoolCreate(NULL, NULL, (__bridge CFDictionaryRef)attrs, &s->_pool);
            if (![s setupEncoderWithError:&e]) {
                set_err(err, err_len, e);
                return NULL;
            }
            s.timer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, s.videoQueue);
            dispatch_source_set_timer(s.timer, DISPATCH_TIME_NOW, NSEC_PER_SEC / fps, NSEC_PER_MSEC);
            __weak PTPSession *weak = s;
            dispatch_source_set_event_handler(s.timer, ^{ [weak testPatternFrame]; });
            dispatch_resume(s.timer);
            if (cfg->audio_mode != PTP_AUDIO_NONE) {
                s.audioTimer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, s.audioQueue);
                dispatch_source_set_timer(s.audioTimer, DISPATCH_TIME_NOW, 10 * NSEC_PER_MSEC, NSEC_PER_MSEC);
                dispatch_source_set_event_handler(s.audioTimer, ^{ [weak testPatternAudio]; });
                dispatch_resume(s.audioTimer);
            }
            *out_width = s->_width;
            *out_height = s->_height;
            return (__bridge_retained void *)s;
        }

        NSError *ne = nil;
        SCShareableContent *content = fetch_content(&ne);
        if (!content) {
            set_err(err, err_len, ne.localizedDescription ?: @"Screen recording permission is required");
            return NULL;
        }

        // ── video stream ──
        SCContentFilter *vfilter = nil;
        double srcW = 0, srcH = 0;
        if (cfg->source_kind == PTP_SOURCE_DISPLAY) {
            SCDisplay *d = find_display(content, cfg->source_id);
            if (!d) {
                set_err(err, err_len, @"Selected display is no longer available");
                return NULL;
            }
            vfilter = [[SCContentFilter alloc] initWithDisplay:d excludingWindows:@[]];
            double sc = display_scale(d.displayID);
            srcW = d.width * sc;
            srcH = d.height * sc;
        } else {
            SCWindow *w = find_window(content, cfg->source_id);
            if (!w) {
                set_err(err, err_len, @"Selected window is no longer available");
                return NULL;
            }
            vfilter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:w];
            double sc = display_scale(CGMainDisplayID());
            srcW = w.frame.size.width * sc;
            srcH = w.frame.size.height * sc;
        }
        fit_size(srcW, srcH, cfg->max_width, cfg->max_height, &s->_width, &s->_height);

        if (![s setupEncoderWithError:&e]) {
            set_err(err, err_len, e);
            return NULL;
        }

        SCStreamConfiguration *vc = [SCStreamConfiguration new];
        vc.width = s->_width;
        vc.height = s->_height;
        vc.minimumFrameInterval = CMTimeMake(1, (int32_t)fps);
        vc.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        vc.colorMatrix = kCGDisplayStreamYCbCrMatrix_ITU_R_709_2;
        // Convert from the display's colour space (Display P3 on Macs with wide-
        // gamut screens) to sRGB/BT.709, which is what browsers assume. Without
        // this P3 values are shown as sRGB: washed out, warm colours look cold.
        vc.colorSpaceName = kCGColorSpaceSRGB;
        vc.queueDepth = 6;
        vc.showsCursor = YES;
        vc.capturesAudio = NO;
        vc.scalesToFit = YES;
        s.videoConfig = vc;
        s->_defaultSourceRect = vc.sourceRect;
        s->_crop = CGRectNull;
        s->_candCrop = CGRectNull;
        s->_isWindow = cfg->source_kind == PTP_SOURCE_WINDOW;
        s->_windowId = cfg->source_id;
        s->_displayId = cfg->source_kind == PTP_SOURCE_DISPLAY ? cfg->source_id : 0;
        s->_lbMemory = CGRectNull;
        s.videoStream = [[SCStream alloc] initWithFilter:vfilter configuration:vc delegate:s];
        if (![s.videoStream addStreamOutput:s type:SCStreamOutputTypeScreen sampleHandlerQueue:s.videoQueue error:&ne]) {
            set_err(err, err_len, ne.localizedDescription);
            return NULL;
        }

        // ── audio stream (separate so audio filtering never affects the picture) ──
        BOOL wantMic = cfg->capture_mic && ptp_mic_supported();
        BOOL wantSystem = cfg->audio_mode != PTP_AUDIO_NONE;
        if (wantSystem || wantMic) {
            SCDisplay *md = main_display(content);
            SCContentFilter *afilter = nil;
            if (cfg->audio_mode == PTP_AUDIO_SINGLE_APP && cfg->audio_app_bundle) {
                SCRunningApplication *app = find_app(content, [NSString stringWithUTF8String:cfg->audio_app_bundle]);
                if (!app) {
                    set_err(err, err_len, @"The app selected for audio is not running");
                    return NULL;
                }
                afilter = [[SCContentFilter alloc] initWithDisplay:md includingApplications:@[ app ] exceptingWindows:@[]];
            } else {
                NSMutableArray *excluded = [NSMutableArray array];
                if (cfg->excluded_bundles) {
                    NSString *list = [NSString stringWithUTF8String:cfg->excluded_bundles];
                    for (NSString *bid in [list componentsSeparatedByString:@"\n"]) {
                        if (bid.length == 0) continue;
                        for (SCRunningApplication *a in content.applications)
                            if ([a.bundleIdentifier isEqualToString:bid] ||
                                [a.bundleIdentifier hasPrefix:[bid stringByAppendingString:@"."]])
                                [excluded addObject:a];
                    }
                }
                afilter = [[SCContentFilter alloc] initWithDisplay:md excludingApplications:excluded exceptingWindows:@[]];
            }
            SCStreamConfiguration *ac = [SCStreamConfiguration new];
            ac.width = 2;
            ac.height = 2;
            ac.minimumFrameInterval = CMTimeMake(1, 1);
            ac.queueDepth = 3;
            ac.showsCursor = NO;
            ac.capturesAudio = wantSystem;
            ac.sampleRate = 48000;
            ac.channelCount = 2;
            ac.excludesCurrentProcessAudio = YES;
            if (wantMic) {
                if (@available(macOS 15.0, *)) ac.captureMicrophone = YES;
            }
            s.audioStream = [[SCStream alloc] initWithFilter:afilter configuration:ac delegate:s];
            [s.audioStream addStreamOutput:s type:SCStreamOutputTypeScreen sampleHandlerQueue:s.audioQueue error:nil];
            if (wantSystem &&
                ![s.audioStream addStreamOutput:s type:SCStreamOutputTypeAudio sampleHandlerQueue:s.audioQueue error:&ne]) {
                set_err(err, err_len, ne.localizedDescription);
                return NULL;
            }
            if (wantMic) {
                if (@available(macOS 15.0, *)) {
                    [s.audioStream addStreamOutput:s type:SCStreamOutputTypeMicrophone sampleHandlerQueue:s.audioQueue error:nil];
                }
            }
        }

        if (!start_stream(s.videoStream, &e)) {
            set_err(err, err_len, e);
            atomic_store(&s->_stopped, true);
            return NULL;
        }
        if (s.audioStream && !start_stream(s.audioStream, &e)) {
            atomic_store(&s->_stopped, true);
            stop_stream(s.videoStream);
            set_err(err, err_len, [@"Audio capture failed: " stringByAppendingString:e ?: @""]);
            return NULL;
        }

        s.timer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, s.videoQueue);
        dispatch_source_set_timer(s.timer, DISPATCH_TIME_NOW, 20 * NSEC_PER_MSEC, 5 * NSEC_PER_MSEC);
        __weak PTPSession *weak = s;
        dispatch_source_set_event_handler(s.timer, ^{ [weak tick]; });
        dispatch_resume(s.timer);

        *out_width = s->_width;
        *out_height = s->_height;
        return (__bridge_retained void *)s;
    }
}

void ptp_request_keyframe(void *handle) {
    if (!handle) return;
    PTPSession *s = (__bridge PTPSession *)handle;
    atomic_store(&s->_forceKey, true);
}

void ptp_set_bitrate(void *handle, uint32_t kbps) {
    if (!handle) return;
    PTPSession *s = (__bridge PTPSession *)handle;
    dispatch_async(s.videoQueue, ^{ [s applyBitrate:kbps]; });
}

void ptp_stop(void *handle) {
    if (!handle) return;
    @autoreleasepool {
        PTPSession *s = (__bridge_transfer PTPSession *)handle;
        atomic_store(&s->_stopped, true);
        if (s.timer) dispatch_source_cancel(s.timer);
        if (s.audioTimer) dispatch_source_cancel(s.audioTimer);
        stop_stream(s.videoStream);
        stop_stream(s.audioStream);
        // Drain queues so no callback is mid-flight once we return.
        dispatch_sync(s.videoQueue, ^{
            if (s->_vt) {
                VTCompressionSessionCompleteFrames(s->_vt, kCMTimeInvalid);
                VTCompressionSessionInvalidate(s->_vt);
                CFRelease(s->_vt);
                s->_vt = NULL;
            }
        });
        dispatch_sync(s.audioQueue, ^{});
    }
}
