#import <AppKit/AppKit.h>
#import <CoreServices/CoreServices.h>
#include <assert.h>
#include <stdbool.h>
extern bool rcu_is_login_event(NSAppleEventDescriptor *event);
extern void rcu_install_events(void (*callback)(void));
@interface RCUApplicationEvents : NSObject
- (void)open:(NSAppleEventDescriptor *)event reply:(NSAppleEventDescriptor *)reply;
- (void)reopen:(NSAppleEventDescriptor *)event reply:(NSAppleEventDescriptor *)reply;
@end
static int shown;
static void show(void) { shown++; }
int main(void) {
    @autoreleasepool {
        NSAppleEventDescriptor *event = [NSAppleEventDescriptor
            appleEventWithEventClass:kCoreEventClass eventID:kAEOpenApplication
            targetDescriptor:nil returnID:kAutoGenerateReturnID transactionID:kAnyTransactionID];
        assert(!rcu_is_login_event(nil));
        assert(!rcu_is_login_event(event));
        rcu_install_events(show);
        RCUApplicationEvents *handler = [RCUApplicationEvents new];
        [handler open:event reply:nil];
        assert(shown == 1);
        [event setParamDescriptor:[NSAppleEventDescriptor descriptorWithEnumCode:keyAELaunchedAsLogInItem]
                      forKeyword:keyAEPropData];
        assert(rcu_is_login_event(event));
        [handler open:event reply:nil];
        assert(shown == 1); // Login must not pop up a window.
        [handler reopen:event reply:nil];
        assert(shown == 2); // Explicit reopening must restore the existing window.
        puts("macOS launch event tests passed");
    }
    return 0;
}
