#import <AVFoundation/AVFoundation.h>
#import <AppKit/AppKit.h>
#import <Speech/Speech.h>

#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

typedef void (*LMNativeDictationCallback)(
    void *user_data,
    const char *kind,
    const char *text,
    const char *code,
    const char *message
);

static const char *lm_utf8(NSString *value) {
    return value.UTF8String ?: "";
}

@interface LMDictationSession : NSObject
@property(nonatomic, copy) NSString *sessionId;
@property(nonatomic, copy) NSString *localeIdentifier;
@property(nonatomic) BOOL requireOnDevice;
@property(nonatomic) LMNativeDictationCallback callback;
@property(nonatomic) void *userData;
@property(nonatomic, strong) SFSpeechRecognizer *recognizer;
@property(nonatomic, strong) SFSpeechAudioBufferRecognitionRequest *request;
@property(nonatomic, strong) SFSpeechRecognitionTask *task;
@property(nonatomic, strong) AVAudioEngine *audioEngine;
@property(nonatomic, strong) AVAudioInputNode *inputNode;
@property(nonatomic, strong) dispatch_queue_t callbackQueue;
@property(nonatomic) dispatch_semaphore_t stopSemaphore;
@property(nonatomic) BOOL stopped;
@property(nonatomic) BOOL stopping;
- (void)failWithCode:(NSString *)code message:(NSString *)message;
- (void)finishStoppingWithError:(NSString *)message;
- (void)cancelRecognitionTask;
@end

@implementation LMDictationSession

- (void)emitKind:(const char *)kind text:(NSString *)text code:(NSString *)code message:(NSString *)message {
    LMNativeDictationCallback callback = self.callback;
    if (!callback) return;
    callback(self.userData, kind, lm_utf8(text), lm_utf8(code), lm_utf8(message));
}

- (void)emitState:(NSString *)state {
    [self emitKind:"state" text:self.sessionId code:state message:@""];
}

- (void)emitError:(NSString *)code message:(NSString *)message {
    [self emitKind:"error" text:code code:self.sessionId message:message];
}

- (void)begin {
    if (self.stopped) return;
    [self emitState:@"starting"];
    __weak LMDictationSession *weakSelf = self;
    [SFSpeechRecognizer requestAuthorization:^(SFSpeechRecognizerAuthorizationStatus status) {
        LMDictationSession *strongSelf = weakSelf;
        if (!strongSelf) return;
        dispatch_queue_t callbackQueue = strongSelf.callbackQueue;
        dispatch_async(callbackQueue, ^{
            if (strongSelf.stopped) return;
            if (status != SFSpeechRecognizerAuthorizationStatusAuthorized) {
                NSString *message = status == SFSpeechRecognizerAuthorizationStatusDenied
                    ? @"Speech recognition permission is disabled."
                    : @"Speech recognition permission is required for composer dictation.";
                [strongSelf failWithCode:@"speech_permission_denied" message:message];
                return;
            }
            [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL granted) {
                LMDictationSession *microphoneSelf = strongSelf;
                if (!microphoneSelf) return;
                dispatch_queue_t microphoneQueue = microphoneSelf.callbackQueue;
                dispatch_async(microphoneQueue, ^{
                    if (microphoneSelf.stopped) return;
                    if (!granted) {
                        [microphoneSelf failWithCode:@"microphone_permission_denied" message:@"Microphone access is disabled."];
                        return;
                    }
                    [microphoneSelf beginRecognition];
                });
            }];
        });
    }];
}

- (void)beginRecognition {
    if (self.stopped) return;
    if (!self.recognizer || !self.recognizer.isAvailable) {
        [self failWithCode:@"speech_unavailable" message:@"Speech recognition is unavailable for this language."];
        return;
    }
    if (self.requireOnDevice && !self.recognizer.supportsOnDeviceRecognition) {
        [self failWithCode:@"on_device_unavailable" message:@"On-device speech recognition is not available for this language on this Mac."];
        return;
    }

    self.request = [[SFSpeechAudioBufferRecognitionRequest alloc] init];
    self.request.shouldReportPartialResults = YES;
    if (@available(macOS 10.15, *)) {
        self.request.requiresOnDeviceRecognition = self.requireOnDevice;
    }

    __weak LMDictationSession *weakSelf = self;
    self.task = [self.recognizer recognitionTaskWithRequest:self.request resultHandler:^(SFSpeechRecognitionResult *result, NSError *error) {
        LMDictationSession *strongSelf = weakSelf;
        if (!strongSelf) return;
        dispatch_queue_t callbackQueue = strongSelf.callbackQueue;
        dispatch_async(callbackQueue, ^{
            if (strongSelf.stopped) return;
            BOOL isFinalResult = NO;
            if (result) {
                NSString *text = result.bestTranscription.formattedString ?: @"";
                if (result.isFinal) {
                    isFinalResult = YES;
                    [strongSelf emitKind:"final" text:text code:strongSelf.sessionId message:@""];
                } else {
                    [strongSelf emitKind:"partial" text:text code:strongSelf.sessionId message:@""];
                }
            }
            if (error) {
                NSString *message = error.localizedDescription ?: @"Speech recognition failed.";
                if (strongSelf.stopping) {
                    [strongSelf finishStoppingWithError:message];
                } else {
                    [strongSelf failWithCode:@"recognition_failed" message:message];
                }
            } else if (isFinalResult && strongSelf.stopping) {
                [strongSelf finishStoppingWithError:nil];
            }
        });
    }];

    self.inputNode = self.audioEngine.inputNode;
    AVAudioFormat *format = [self.inputNode outputFormatForBus:0];
    if (!format || format.sampleRate <= 0) {
        [self failWithCode:@"microphone_unavailable" message:@"The system microphone is unavailable."];
        return;
    }
    [self.inputNode installTapOnBus:0 bufferSize:1024 format:format block:^(AVAudioPCMBuffer *buffer, AVAudioTime *_when) {
        (void)_when;
        LMDictationSession *strongSelf = weakSelf;
        if (!strongSelf || strongSelf.stopped) return;
        [strongSelf.request appendAudioPCMBuffer:buffer];
    }];
    [self.audioEngine prepare];
    NSError *startError = nil;
    if (![self.audioEngine startAndReturnError:&startError]) {
        [self failWithCode:@"microphone_start_failed" message:startError.localizedDescription ?: @"The system microphone could not start."];
        return;
    }
    [self emitState:@"listening"];
}

- (void)removeAudio {
    if (self.inputNode) {
        [self.inputNode removeTapOnBus:0];
    }
    [self.audioEngine stop];
    [self.request endAudio];
}

- (void)cancelRecognitionTask {
    [self.task cancel];
    self.task = nil;
    self.request = nil;
}

- (void)failWithCode:(NSString *)code message:(NSString *)message {
    if (self.stopped) return;
    [self removeAudio];
    self.stopped = YES;
    [self cancelRecognitionTask];
    [self emitError:code message:message];
    [self emitState:@"idle"];
}

- (void)finishStoppingWithError:(NSString *)message {
    @synchronized (self) {
        if (self.stopped) return;
        if (message.length > 0) {
            [self emitError:@"recognition_failed" message:message];
        }
        self.stopped = YES;
        [self cancelRecognitionTask];
        [self emitState:@"idle"];
        dispatch_semaphore_signal(self.stopSemaphore);
    }
}

- (void)stop {
    BOOL shouldWaitForRecognition = NO;
    @synchronized (self) {
        if (self.stopped || self.stopping) return;
        self.stopping = YES;
        [self emitState:@"stopping"];
        [self removeAudio];
        shouldWaitForRecognition = self.task != nil;
        if (!shouldWaitForRecognition) {
            self.stopped = YES;
            [self emitState:@"idle"];
        }
    }
    if (!shouldWaitForRecognition) return;

    dispatch_time_t deadline = dispatch_time(DISPATCH_TIME_NOW, (int64_t)(5 * NSEC_PER_SEC));
    if (dispatch_semaphore_wait(self.stopSemaphore, deadline) != 0) {
        @synchronized (self) {
            if (!self.stopped) {
                self.stopped = YES;
                [self cancelRecognitionTask];
                [self emitError:@"recognition_timeout" message:@"Speech recognition did not finish before stopping."];
                [self emitState:@"idle"];
            }
        }
        dispatch_sync(self.callbackQueue, ^{});
    }
}

- (void)cancel {
    @synchronized (self) {
        if (self.stopped) {
            [self removeAudio];
            return;
        }
        self.stopping = YES;
        self.stopped = YES;
        [self removeAudio];
        [self cancelRecognitionTask];
    }
    dispatch_sync(self.callbackQueue, ^{});
    [self emitState:@"idle"];
}

@end

int little_monkey_dictation_macos_start(
    const char *session_id,
    const char *locale,
    bool require_on_device,
    LMNativeDictationCallback callback,
    void *user_data,
    void **out_session
) {
    if (!session_id || !callback || !out_session) return -1;
    @autoreleasepool {
        NSString *localeIdentifier = locale && locale[0] != '\0'
            ? [NSString stringWithUTF8String:locale]
            : [[NSLocale currentLocale] localeIdentifier];
        NSLocale *nativeLocale = [[NSLocale alloc] initWithLocaleIdentifier:localeIdentifier];
        SFSpeechRecognizer *recognizer = [[SFSpeechRecognizer alloc] initWithLocale:nativeLocale];
        if (!recognizer) return -2;
        LMDictationSession *session = [[LMDictationSession alloc] init];
        session.sessionId = [NSString stringWithUTF8String:session_id];
        session.localeIdentifier = localeIdentifier;
        session.requireOnDevice = require_on_device;
        session.callback = callback;
        session.userData = user_data;
        session.recognizer = recognizer;
        session.audioEngine = [[AVAudioEngine alloc] init];
        session.callbackQueue = dispatch_queue_create("com.littlemonkey.dictation", DISPATCH_QUEUE_SERIAL);
        session.stopSemaphore = dispatch_semaphore_create(0);
        *out_session = (__bridge_retained void *)session;
        [session begin];
        return 0;
    }
}

void little_monkey_dictation_macos_stop(void *opaque_session) {
    if (!opaque_session) return;
    @autoreleasepool {
        LMDictationSession *session = (__bridge LMDictationSession *)opaque_session;
        [session stop];
    }
}

void little_monkey_dictation_macos_cancel(void *opaque_session) {
    if (!opaque_session) return;
    @autoreleasepool {
        LMDictationSession *session = (__bridge LMDictationSession *)opaque_session;
        [session cancel];
    }
}

void little_monkey_dictation_macos_release(void *opaque_session) {
    if (!opaque_session) return;
    @autoreleasepool {
        (void)(__bridge_transfer LMDictationSession *)opaque_session;
    }
}

bool little_monkey_dictation_macos_open_permission_settings(const char *kind) {
    if (!kind) return false;
    NSString *permissionKind = [NSString stringWithUTF8String:kind];
    NSString *urlString = nil;
    if ([permissionKind isEqualToString:@"microphone"]) {
        urlString = @"x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";
    } else if ([permissionKind isEqualToString:@"speech"]) {
        urlString = @"x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition";
    } else {
        return false;
    }
    NSURL *url = [NSURL URLWithString:urlString];
    return url != nil && [[NSWorkspace sharedWorkspace] openURL:url];
}

static NSString *lm_speech_permission_status(void) {
    switch ([SFSpeechRecognizer authorizationStatus]) {
        case SFSpeechRecognizerAuthorizationStatusAuthorized:
            return @"granted";
        case SFSpeechRecognizerAuthorizationStatusDenied:
            return @"denied";
        case SFSpeechRecognizerAuthorizationStatusRestricted:
            return @"restricted";
        case SFSpeechRecognizerAuthorizationStatusNotDetermined:
            return @"notDetermined";
    }
    return @"unknown";
}

static NSString *lm_microphone_permission_status(void) {
    switch ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]) {
        case AVAuthorizationStatusAuthorized:
            return @"granted";
        case AVAuthorizationStatusDenied:
            return @"denied";
        case AVAuthorizationStatusRestricted:
            return @"restricted";
        case AVAuthorizationStatusNotDetermined:
            return @"notDetermined";
    }
    return @"unknown";
}

char *little_monkey_dictation_macos_capabilities_json(void) {
    @autoreleasepool {
        NSMutableArray *languages = [NSMutableArray array];
        SFSpeechRecognizer *defaultRecognizer = [[SFSpeechRecognizer alloc] initWithLocale:[NSLocale currentLocale]];
        BOOL defaultSupportsOnDevice = defaultRecognizer != nil && defaultRecognizer.supportsOnDeviceRecognition;
        for (NSLocale *locale in [SFSpeechRecognizer supportedLocales]) {
            NSString *identifier = locale.localeIdentifier ?: @"";
            NSString *label = [locale displayNameForKey:NSLocaleIdentifier value:identifier] ?: identifier;
            SFSpeechRecognizer *recognizer = [[SFSpeechRecognizer alloc] initWithLocale:locale];
            BOOL supportsOnDevice = recognizer != nil && recognizer.supportsOnDeviceRecognition;
            [languages addObject:@{
                @"id": identifier,
                @"label": label,
                @"supportsOnDevice": @(supportsOnDevice),
            }];
        }
        BOOL supported = defaultRecognizer != nil;
        NSData *data = [NSJSONSerialization dataWithJSONObject:@{
            @"supported": @(supported),
            @"supportsPartialResults": @YES,
            @"supportsOnDevice": @(defaultSupportsOnDevice),
            @"languages": languages,
            @"permissions": @{
                @"microphone": lm_microphone_permission_status(),
                @"speech": lm_speech_permission_status(),
            },
        } options:0 error:nil];
        char *result = malloc(data.length + 1);
        if (!result) return NULL;
        memcpy(result, data.bytes, data.length);
        result[data.length] = '\0';
        return result;
    }
}

void little_monkey_dictation_macos_free_string(char *value) {
    free(value);
}
