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

@interface PTPSession : NSObject <SCStreamOutput, SCStreamDelegate> {
  @public
    ptp_config _cfg;
    atomic_bool _stopped;
    atomic_bool _forceKey;
    VTCompressionSessionRef _vt;
    CVPixelBufferRef _lastFrame;
    int64_t _lastEncodeUs;
    int64_t _lastPtsUs;
    uint32_t _width, _height;
    // window sources: follow the window's size
    BOOL _isWindow;
    uint32_t _windowId;
    double _scaleFactor;
    uint32_t _candW, _candH;
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

@implementation PTPSession

- (void)dealloc {
    if (_lastFrame) CVPixelBufferRelease(_lastFrame);
    if (_pool) CVPixelBufferPoolRelease(_pool);
}

- (BOOL)setupEncoderWithError:(NSString **)error {
    NSDictionary *spec = @{(id)kVTVideoEncoderSpecification_EnableLowLatencyRateControl : @YES};
    OSStatus s = VTCompressionSessionCreate(NULL, _width, _height, kCMVideoCodecType_H264, (__bridge CFDictionaryRef)spec,
                                            NULL, NULL, vt_output, (__bridge void *)self, &_vt);
    if (s != noErr) {
        s = VTCompressionSessionCreate(NULL, _width, _height, kCMVideoCodecType_H264, NULL, NULL, NULL, vt_output,
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
    if (atomic_load(&_stopped) || !_vt || !pb) return;
    // Right after a resize, a few frames may still arrive at the old size.
    if (CVPixelBufferGetWidth(pb) != _width || CVPixelBufferGetHeight(pb) != _height) return;
    int64_t now = host_time_us();
    int64_t pts = now;
    if (pts <= _lastPtsUs) pts = _lastPtsUs + 1000;
    _lastPtsUs = pts;
    _lastEncodeUs = now;
    NSDictionary *opts = nil;
    if (atomic_exchange(&_forceKey, false)) opts = @{(id)kVTEncodeFrameOptionKey_ForceKeyFrame : @YES};
    VTCompressionSessionEncodeFrame(_vt, pb, CMTimeMake(pts, 1000000), kCMTimeInvalid, (__bridge CFDictionaryRef)opts,
                                    NULL, NULL);
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
    if (!_lastFrame) return;
    int64_t idle = host_time_us() - _lastEncodeUs;
    if (idle > 500000 || (atomic_load(&_forceKey) && idle > 40000)) [self encode:_lastFrame];
}

static void fit_size(double srcW, double srcH, uint32_t maxW, uint32_t maxH, uint32_t *w, uint32_t *h);

// Window sources: the stream size is fixed at start, so when the window is
// resized ScreenCaptureKit shrinks it inside the old frame (black borders).
// Follow the window instead: once a new size has been stable for 300 ms,
// resize the capture and restart the encoder (viewers get a new keyframe).
// The size comes from ScreenCaptureKit's own frame metadata (not the window
// server), so it matches what is actually captured, including fullscreen
// windows, scaled displays and windows moved between displays.
// Must be called on videoQueue.
- (void)checkWindowSize {
    int64_t now = host_time_us();
    if (now - _lastSizeCheckUs < 100000) return;
    _lastSizeCheckUs = now;
    if (_srcW < 16 || _srcH < 16) return;
    uint32_t nw, nh;
    fit_size(_srcW, _srcH, _cfg.max_width, _cfg.max_height, &nw, &nh);
    if (nw == _width && nh == _height) {
        _candW = _candH = 0;
        return;
    }
    if (nw != _candW || nh != _candH) {
        _candW = nw;
        _candH = nh;
        _candSinceUs = now;
        return;
    }
    if (now - _candSinceUs < 300000) return;
    _candW = _candH = 0;
    [self resizeTo:nw height:nh];
}

- (void)resizeTo:(uint32_t)w height:(uint32_t)h {
    uint32_t oldW = _width, oldH = _height;
    if (_vt) {
        VTCompressionSessionCompleteFrames(_vt, kCMTimeInvalid);
        VTCompressionSessionInvalidate(_vt);
        CFRelease(_vt);
        _vt = NULL;
    }
    _width = w;
    _height = h;
    NSString *e = nil;
    if (![self setupEncoderWithError:&e]) {
        // Fall back to the old size rather than stopping the stream.
        _width = oldW;
        _height = oldH;
        [self setupEncoderWithError:&e];
        return;
    }
    atomic_store(&_forceKey, true);
    [self rememberFrame:NULL];
    SCStreamConfiguration *c = self.videoConfig;
    c.width = w;
    c.height = h;
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
        [self rememberFrame:pb];
        [self encode:pb];
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
        vc.queueDepth = 6;
        vc.showsCursor = YES;
        vc.capturesAudio = NO;
        vc.scalesToFit = YES;
        s.videoConfig = vc;
        s->_isWindow = cfg->source_kind == PTP_SOURCE_WINDOW;
        s->_windowId = cfg->source_id;
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
