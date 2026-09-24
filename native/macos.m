#import <AppKit/AppKit.h>
#import <CoreServices/CoreServices.h>
#import <ServiceManagement/ServiceManagement.h>
#include <stdbool.h>
#include <stdio.h>

static NSString *const RCUIdentifier = @"com.doulongfei.clipboard-uploader";
static void (*showWindow)(void);
static BOOL receivedOpenEvent;
static BOOL startupShouldShow;

bool rcu_is_bundle(void) {
    NSBundle *bundle = NSBundle.mainBundle;
    return [bundle.bundleIdentifier isEqualToString:RCUIdentifier] &&
           [bundle.bundlePath.pathExtension isEqualToString:@"app"];
}

// Apple documents the login marker as the keyAEPropData parameter of oapp.
bool rcu_is_login_event(NSAppleEventDescriptor *event) {
    return event.eventID == kAEOpenApplication &&
        [[event paramDescriptorForKeyword:keyAEPropData] enumCodeValue] ==
            keyAELaunchedAsLogInItem;
}

@interface RCUApplicationEvents : NSObject
@end

@implementation RCUApplicationEvents
- (void)installHandlers:(NSNotification *)notification {
    (void)notification;
    NSAppleEventManager *manager = NSAppleEventManager.sharedAppleEventManager;
    [manager setEventHandler:self andSelector:@selector(open:reply:)
              forEventClass:kCoreEventClass andEventID:kAEOpenApplication];
    [manager setEventHandler:self andSelector:@selector(reopen:reply:)
              forEventClass:kCoreEventClass andEventID:kAEReopenApplication];
}
- (void)open:(NSAppleEventDescriptor *)event reply:(NSAppleEventDescriptor *)reply {
    (void)reply;
    receivedOpenEvent = YES;
    if (!rcu_is_login_event(event) && showWindow) showWindow();
}
- (void)reopen:(NSAppleEventDescriptor *)event reply:(NSAppleEventDescriptor *)reply {
    (void)event;
    (void)reply;
    if (showWindow) showWindow();
}
- (void)didFinish:(NSNotification *)notification {
    (void)notification;
    // Command-line launches may have no open Apple event.
    if (!receivedOpenEvent &&
        !rcu_is_login_event(NSAppleEventManager.sharedAppleEventManager.currentAppleEvent) &&
        showWindow) showWindow();
}
@end

void rcu_install_events(void (*callback)(void)) {
    showWindow = callback;
    static RCUApplicationEvents *events;
    events = [RCUApplicationEvents new];
    NSNotificationCenter *center = NSNotificationCenter.defaultCenter;
    [center addObserver:events selector:@selector(installHandlers:)
                  name:NSApplicationWillFinishLaunchingNotification object:nil];
    [center addObserver:events selector:@selector(didFinish:)
                  name:NSApplicationDidFinishLaunchingNotification object:nil];
    [events installHandlers:nil];
}

void rcu_activate(void) {
    startupShouldShow = YES;
    // Activation of an accessory app must be requested explicitly.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    [NSApp activateIgnoringOtherApps:YES];
#pragma clang diagnostic pop
}

// eframe 0.29 orders the root window front after its first paint even when the
// viewport requests visible=false. Mask that paint, then apply our launch policy
// on the main queue after painting finishes. This prevents a login-time flash.
void rcu_prepare_window(void) {
    for (NSWindow *window in NSApp.windows) {
        if (window.styleMask & NSWindowStyleMaskTitled) window.alphaValue = 0;
    }
}

void rcu_finish_first_frame(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        for (NSWindow *window in NSApp.windows) {
            if (!(window.styleMask & NSWindowStyleMaskTitled)) continue;
            if (!startupShouldShow) [window orderOut:nil];
            window.alphaValue = 1;
        }
    });
}

void rcu_use_regular_policy(void) {
    [NSApplication.sharedApplication setActivationPolicy:NSApplicationActivationPolicyRegular];
}

// Negative values are local preconditions, nonnegative values are Apple's status.
int rcu_login_status(void) {
    if (!rcu_is_bundle()) return -2;
    if ([NSBundle.mainBundle.bundlePath hasPrefix:@"/Volumes/"]) return -3;
    if (@available(macOS 13.0, *)) return (int)SMAppService.mainAppService.status;
    return -1;
}

int rcu_set_login(bool enabled, char *message, size_t capacity) {
    @autoreleasepool {
        if (rcu_login_status() < 0) {
            snprintf(message, capacity, "%s", "请使用 macOS 13 或更新版本，并先将应用拖入“应用程序”。");
            return -1;
        }
        if (@available(macOS 13.0, *)) {
            SMAppService *service = SMAppService.mainAppService;
            if ((enabled && service.status == SMAppServiceStatusEnabled) ||
                (!enabled && service.status == SMAppServiceStatusNotRegistered)) return 0;
            NSError *error = nil;
            BOOL success = enabled ? [service registerAndReturnError:&error]
                                   : [service unregisterAndReturnError:&error];
            if (!success) {
                snprintf(message, capacity, "%s", error.localizedDescription.UTF8String ?: "登录项操作失败");
                return -1;
            }
            return 0;
        }
        return -1;
    }
}

void rcu_open_login_settings(void) {
    if (@available(macOS 13.0, *)) [SMAppService openSystemSettingsLoginItems];
}

void rcu_show_error(const char *message) {
    NSAlert *alert = [NSAlert new];
    alert.messageText = @"剪贴板上传工具无法启动";
    alert.informativeText = [NSString stringWithUTF8String:message] ?: @"未知错误";
    [alert runModal];
}
