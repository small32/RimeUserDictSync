// RimeMenuBar — 常驻菜单栏助手（由 launchd 的 LaunchAgent 拉起）。
//
// 为什么需要它：鼠须管的菜单把「同步用户数据」和「重新部署」拆成两项，
// 每次都要点两下。这个助手在菜单栏放一个齿轮图标，左键点一下就把两条
// RIME 命令按顺序执行完；右键才是菜单。
//
// 它**不改鼠须管**，只调用 Squirrel 自带的命令行入口：
//     Squirrel --sync     （同步用户资料）
//     Squirrel --reload   （重新部署）
//
// 图标显隐跟随 RimeUserDictSync.ini 的 [background] enabled：
// 后台活动关掉后 5 秒内移除图标并退出；LaunchAgent 用
// KeepAlive={SuccessfulExit:false}，所以正常退出不会被 launchd 重新拉起。
// 重新开启时由主程序 kickstart 这个 job。
//
// 用 Objective-C 而不是 Swift：这台构建机只有 Command Line Tools，
// Swift 的兼容库没有 x86_64 切片，交叉链接会失败；clang 两个架构都能编。

#import <Cocoa/Cocoa.h>
#include <signal.h>

@interface MenuBarAgent : NSObject <NSApplicationDelegate>
@property(nonatomic, strong) NSStatusItem *statusItem;
@property(nonatomic, strong) NSTimer *pollTimer;
@property(nonatomic, strong) NSPopover *progressPopover;
@property(nonatomic, strong) NSProgressIndicator *progressBar;
@property(nonatomic, strong) NSTextField *progressLabel;
@property(nonatomic, strong) dispatch_source_t sigtermSource;
@property(nonatomic, copy) NSString *appDir;
@property(nonatomic) BOOL syncing;
@end

@implementation MenuBarAgent

- (instancetype)initWithAppDir:(NSString *)appDir {
  self = [super init];
  if (self) {
    _appDir = [appDir copy];
  }
  return self;
}

- (void)applicationDidFinishLaunching:(NSNotification *)notification {
  [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
  if (![self backgroundEnabled]) {
    dispatch_async(dispatch_get_main_queue(), ^{
      [NSApp terminate:nil];
    });
    return;
  }
  [self installStatusItem];
  self.pollTimer = [NSTimer scheduledTimerWithTimeInterval:5
                                                   target:self
                                                 selector:@selector(pollSettings)
                                                 userInfo:nil
                                                  repeats:YES];

  // 收到 SIGTERM 时走正常的 terminate 流程，让 AppKit 把状态项摘掉。
  // 直接被杀会留下"幽灵图标"，只能注销才能清掉（macOS 26 的坑）。
  signal(SIGTERM, SIG_IGN);
  self.sigtermSource =
      dispatch_source_create(DISPATCH_SOURCE_TYPE_SIGNAL, SIGTERM, 0, dispatch_get_main_queue());
  __weak typeof(self) weakSelf = self;
  dispatch_source_set_event_handler(self.sigtermSource, ^{
    [NSApp terminate:nil];
    (void)weakSelf;
  });
  dispatch_resume(self.sigtermSource);

#ifdef RMB_DEMO
  dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(1.0 * NSEC_PER_SEC)),
                 dispatch_get_main_queue(), ^{
                   [self showProgressPopover];
                   [self updateProgress:@"正在同步用户数据…" value:0.15];
                   dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(1.2 * NSEC_PER_SEC)),
                                  dispatch_get_main_queue(), ^{
                                    [self updateProgress:@"正在重新部署…" value:0.6];
                                  });
                   dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(2.4 * NSEC_PER_SEC)),
                                  dispatch_get_main_queue(), ^{
                                    [self updateProgress:@"已触发同步与部署" value:1.0];
                                  });
                 });
#endif
}

- (void)applicationWillTerminate:(NSNotification *)notification {
  if (self.statusItem != nil) {
    [[NSStatusBar systemStatusBar] removeStatusItem:self.statusItem];
    self.statusItem = nil;
  }
}

#pragma mark - 菜单栏图标

- (void)installStatusItem {
  NSStatusItem *item = [[NSStatusBar systemStatusBar]
      statusItemWithLength:NSVariableStatusItemLength];
  // 让系统记住这一项的槽位；没有 autosaveName 时，macOS 26 会把它丢到
  // 屏幕上方 (0, -22) 的"收纳区"，表现就是进程活着但菜单栏没有图标。
  item.autosaveName = @"com.small32.rime-user-dict-sync.menubar.status";
  NSStatusBarButton *button = item.button;
  NSImage *image = [self gearImage];
  image.size = NSMakeSize(18, 18);
  image.template = YES;
  button.image = image;
  if (image == nil) {
    // 图标文件缺失或 SF Symbol 不可用时兜底：宁可显示文字，也不要变成零宽的隐形图标。
    button.title = @"RIME";
  }
  button.toolTip = @"Rime词库同步：点一下同步并部署";
  button.target = self;
  button.action = @selector(statusItemClicked);
  [button sendActionOn:(NSEventMaskLeftMouseUp | NSEventMaskRightMouseUp)];
  self.statusItem = item;
  // macOS 26 上只设 image 时状态栏窗口可能一直不实体化（进程活着、图标不见）。
  // 摸一下 window 会强制它挂到菜单栏。
  (void)item.button.window;
}

- (void)statusItemClicked {
  if (NSApp.currentEvent.type == NSEventTypeRightMouseUp) {
    [self showMenu];
  } else {
    [self triggerSyncAndDeploy];
  }
}

- (void)showMenu {
  NSMenu *menu = [[NSMenu alloc] init];

  NSMenuItem *sync = [[NSMenuItem alloc] initWithTitle:@"同步并部署"
                                                action:@selector(menuSync)
                                         keyEquivalent:@""];
  sync.target = self;
  [menu addItem:sync];
  [menu addItem:[NSMenuItem separatorItem]];

  NSMenuItem *open = [[NSMenuItem alloc] initWithTitle:@"打开 RimeUserDictSync"
                                                action:@selector(menuOpenApp)
                                         keyEquivalent:@""];
  open.target = self;
  [menu addItem:open];
  [menu addItem:[NSMenuItem separatorItem]];

  NSMenuItem *hide = [[NSMenuItem alloc] initWithTitle:@"隐藏图标（下次登录恢复）"
                                                action:@selector(menuQuit)
                                         keyEquivalent:@""];
  hide.target = self;
  [menu addItem:hide];

  self.statusItem.menu = menu;
  [self.statusItem.button performClick:nil];
  self.statusItem.menu = nil;
}

- (void)menuSync {
  [self triggerSyncAndDeploy];
}

- (void)menuOpenApp {
  NSURL *url = [NSURL fileURLWithPath:@"/Applications/RimeUserDictSync.app"];
  [[NSWorkspace sharedWorkspace] openURL:url];
}

- (void)menuQuit {
  [NSApp terminate:nil];
}

#pragma mark - 执行 RIME 两条命令

- (void)triggerSyncAndDeploy {
  if (self.syncing) {
    return;
  }
  NSString *executable = [self squirrelPath];
  if (![[NSFileManager defaultManager] isExecutableFileAtPath:executable]) {
    [self showError:[NSString stringWithFormat:@"找不到鼠须管：\n%@", executable]];
    return;
  }
  self.syncing = YES;
  self.statusItem.button.appearsDisabled = YES;
  [self showProgressPopover];
  [self updateProgress:@"正在同步用户数据…" value:0.1];

  dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
    [self run:executable arguments:@[ @"--sync" ]];
    // 命令行只是投递通知，给用户一个能看清的阶段反馈，再进入下一步。
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(0.9 * NSEC_PER_SEC)),
                   dispatch_get_main_queue(), ^{
                     [self updateProgress:@"正在重新部署…" value:0.55];
                     dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
                       [self run:executable arguments:@[ @"--reload" ]];
                       dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(0.9 * NSEC_PER_SEC)),
                                      dispatch_get_main_queue(), ^{
                                        [self updateProgress:@"已触发同步与部署" value:1.0];
                                        dispatch_after(
                                            dispatch_time(DISPATCH_TIME_NOW,
                                                          (int64_t)(0.8 * NSEC_PER_SEC)),
                                            dispatch_get_main_queue(), ^{
                                              [self hideProgressPopover];
                                              self.syncing = NO;
                                              self.statusItem.button.appearsDisabled = NO;
                                            });
                                      });
                     });
                   });
  });
}

#pragma mark - 进度浮层

- (void)showProgressPopover {
  if (self.progressPopover == nil) {
    [self buildProgressPopover];
  }
  [self.progressPopover showRelativeToRect:self.statusItem.button.bounds
                                    ofView:self.statusItem.button
                             preferredEdge:NSRectEdgeMinY];
}

- (void)buildProgressPopover {
  NSView *view = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, 260, 76)];

  NSTextField *label = [NSTextField labelWithString:@"正在同步用户数据…"];
  label.frame = NSMakeRect(16, 44, 228, 18);
  label.font = [NSFont systemFontOfSize:13 weight:NSFontWeightMedium];
  [view addSubview:label];

  NSProgressIndicator *bar =
      [[NSProgressIndicator alloc] initWithFrame:NSMakeRect(16, 20, 228, 12)];
  bar.style = NSProgressIndicatorStyleBar;
  bar.indeterminate = NO;
  bar.minValue = 0.0;
  bar.maxValue = 1.0;
  bar.doubleValue = 0.0;
  [view addSubview:bar];

  NSViewController *controller = [[NSViewController alloc] init];
  controller.view = view;

  NSPopover *popover = [[NSPopover alloc] init];
  popover.contentViewController = controller;
  popover.contentSize = NSMakeSize(260, 76);
  popover.behavior = NSPopoverBehaviorTransient;
  popover.animates = YES;

  self.progressPopover = popover;
  self.progressLabel = label;
  self.progressBar = bar;
}

- (void)updateProgress:(NSString *)text value:(double)value {
  if (self.progressLabel == nil) {
    return;
  }
  self.progressLabel.stringValue = text;
  [[self.progressBar animator] setDoubleValue:value];
}

- (void)hideProgressPopover {
  [self.progressPopover performClose:nil];
}

- (BOOL)run:(NSString *)path arguments:(NSArray<NSString *> *)arguments {
  NSTask *task = [[NSTask alloc] init];
  task.executableURL = [NSURL fileURLWithPath:path];
  task.arguments = arguments;
  task.standardOutput = [NSFileHandle fileHandleWithNullDevice];
  task.standardError = [NSFileHandle fileHandleWithNullDevice];
  NSError *error = nil;
  if (![task launchAndReturnError:&error]) {
    return NO;
  }
  [task waitUntilExit];
  return task.terminationStatus == 0;
}

- (void)showError:(NSString *)message {
  NSAlert *alert = [[NSAlert alloc] init];
  alert.messageText = @"Rime词库同步";
  alert.informativeText = message;
  alert.alertStyle = NSAlertStyleWarning;
  [alert addButtonWithTitle:@"好"];
  [NSApp activateIgnoringOtherApps:YES];
  [alert runModal];
}

#pragma mark - 设置与图标

- (void)pollSettings {
  if (![self settingsFileExists] || ![self backgroundEnabled]) {
    if (self.statusItem) {
      [[NSStatusBar systemStatusBar] removeStatusItem:self.statusItem];
      self.statusItem = nil;
    }
    [NSApp terminate:nil];
  }
}

- (BOOL)settingsFileExists {
  NSString *path = [self.appDir stringByAppendingPathComponent:@"RimeUserDictSync.ini"];
  return [[NSFileManager defaultManager] fileExistsAtPath:path];
}

- (BOOL)backgroundEnabled {
  if (![self settingsFileExists]) {
    return NO;
  }
  NSString *value = [self iniValueForSection:@"background" key:@"enabled"];
  if (value == nil) {
    return YES;
  }
  NSString *normalized = value.lowercaseString;
  return !([normalized isEqualToString:@"0"] || [normalized isEqualToString:@"false"] ||
           [normalized isEqualToString:@"no"] || [normalized isEqualToString:@"off"]);
}

- (NSString *)squirrelPath {
  NSString *configured = [self iniValueForSection:@"rime" key:@"deployer_path"];
  if (configured != nil &&
      [configured stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceCharacterSet]].length > 0) {
    return configured;
  }
  return @"/Library/Input Methods/Squirrel.app/Contents/MacOS/Squirrel";
}

- (NSString *)iniValueForSection:(NSString *)section key:(NSString *)key {
  NSString *path = [self.appDir stringByAppendingPathComponent:@"RimeUserDictSync.ini"];
  NSString *text = [NSString stringWithContentsOfFile:path
                                             encoding:NSUTF8StringEncoding
                                                error:NULL];
  if (text == nil) {
    return nil;
  }
  NSCharacterSet *whitespace = [NSCharacterSet whitespaceCharacterSet];
  NSString *current = @"";
  for (NSString *rawLine in [text componentsSeparatedByCharactersInSet:
                                      [NSCharacterSet newlineCharacterSet]]) {
    NSString *line = [rawLine stringByTrimmingCharactersInSet:whitespace];
    if (line.length == 0 || [line hasPrefix:@"#"] || [line hasPrefix:@";"]) {
      continue;
    }
    if ([line hasPrefix:@"["] && [line hasSuffix:@"]"]) {
      current = [line substringWithRange:NSMakeRange(1, line.length - 2)];
      continue;
    }
    if (![current isEqualToString:section]) {
      continue;
    }
    NSRange equal = [line rangeOfString:@"="];
    if (equal.location == NSNotFound) {
      continue;
    }
    NSString *name = [[line substringToIndex:equal.location]
        stringByTrimmingCharactersInSet:whitespace];
    if (![name isEqualToString:key]) {
      continue;
    }
    return [[line substringFromIndex:equal.location + 1]
        stringByTrimmingCharactersInSet:whitespace];
  }
  return nil;
}

- (NSImage *)gearImage {
  NSURL *executable = [[NSURL fileURLWithPath:NSProcessInfo.processInfo.arguments[0]]
      URLByResolvingSymlinksInPath];
  NSURL *resources = [[executable URLByDeletingLastPathComponent] URLByDeletingLastPathComponent];
  NSString *path =
      [[resources URLByAppendingPathComponent:@"Resources/sync-gear-template.png"] path];
  NSImage *image = [[NSImage alloc] initWithContentsOfFile:path];
  if (image != nil) {
    return image;
  }
  NSImage *fallback = [NSImage imageWithSystemSymbolName:@"gearshape"
                                accessibilityDescription:@"Rime词库同步"];
  return fallback;
}

@end

static NSString *AppDirectoryFromArguments(NSArray<NSString *> *arguments) {
  NSUInteger index = [arguments indexOfObject:@"--app-dir"];
  if (index != NSNotFound && index + 1 < arguments.count) {
    return arguments[index + 1];
  }
  return [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/RimeUserDictSync"];
}

int main(int argc, const char *argv[]) {
  @autoreleasepool {
    NSApplication *application = [NSApplication sharedApplication];
    // NSApplication.delegate 是 weak：这里必须自己拿住，否则 ARC 可能在
    // 赋值后就把 delegate 释放掉，applicationDidFinishLaunching 永远不来，
    // 结果就是进程活着但没有状态栏图标。
    static MenuBarAgent *agent;
    agent = [[MenuBarAgent alloc] initWithAppDir:AppDirectoryFromArguments(
                                                   NSProcessInfo.processInfo.arguments)];
    application.delegate = agent;
    [application run];
  }
  return 0;
}
