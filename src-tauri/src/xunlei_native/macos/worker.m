// Private, newline-delimited JSON bridge to the user's installed Thunder XPC service.
// Credentials arrive over stdin only. Replies contain no URLs or authorization material.
#import <Foundation/Foundation.h>

@protocol DownloadServiceProtocol
- (void)getVersionwithReply:(void (^)(NSString *))reply;
- (void)initETM:(NSDictionary *)context withReply:(void (^)(int))reply;
- (void)uninitETM;
- (void)setUserInfo:(NSDictionary *)user withReply:(void (^)(int))reply;
- (void)setMaxDownloadSpeed:(long long)download maxUploadSpeed:(long long)upload withReply:(void (^)(int))reply;
- (void)createTask:(NSDictionary *)task withReply:(void (^)(int, int))reply;
- (void)getTaskInfo:(unsigned int)task withReply:(void (^)(NSDictionary *))reply;
- (void)getTaskResourceRecvBytes:(unsigned int)task withReply:(void (^)(NSDictionary *))reply;
- (void)setAccelerateToken:(NSString *)token accelerateType:(int)kind taskId:(unsigned int)task subIndex:(unsigned int)index withReply:(void (^)(int))reply;
- (void)changeTasksOriginURL:(NSString *)url taskId:(unsigned int)task subId:(int)sub withReply:(void (^)(int))reply;
- (void)pauseTasks:(NSArray *)tasks withReply:(void (^)(int))reply;
- (void)resumeTasks:(NSArray *)tasks withReply:(void (^)(int))reply;
- (void)destroyTasks:(NSArray *)tasks shouldRemoveFile:(BOOL)remove withReply:(void (^)(int))reply;
@end
@protocol DownloadHostAgent
- (void)onFileNameChange:(NSDictionary *)event;
- (void)onTaskStateChange:(NSDictionary *)event;
- (void)onDatabaseErrorOccurred:(NSDictionary *)event;
- (void)onSubTaskStateChange:(NSDictionary *)event;
@end
@interface HostAgent : NSObject<DownloadHostAgent> @end
@implementation HostAgent
- (void)onFileNameChange:(NSDictionary *)event {}
- (void)onTaskStateChange:(NSDictionary *)event {}
- (void)onDatabaseErrorOccurred:(NSDictionary *)event {}
- (void)onSubTaskStateChange:(NSDictionary *)event {}
@end

static void output(NSDictionary *value) {
    NSData *data = [NSJSONSerialization dataWithJSONObject:value options:0 error:NULL];
    fwrite(data.bytes, 1, data.length, stdout); fputc('\n', stdout); fflush(stdout);
}
static NSDictionary *infoFor(id<DownloadServiceProtocol> proxy, unsigned int task) {
    __block NSDictionary *info = nil;
    [proxy getTaskInfo:task withReply:^(NSDictionary *value) { info = value; }];
    return info ?: @{};
}
static int pauseAndWait(id<DownloadServiceProtocol> proxy, NSArray *tasks) {
    if (!tasks.count) return 0;
    __block int result = -1;
    [proxy pauseTasks:tasks withReply:^(int code) { result = code; }];
    if (result != 0) return result;
    for (int i = 0; i < 20; i++) {
        BOOL running = NO;
        for (NSNumber *task in tasks) {
            if ([infoFor(proxy, task.unsignedIntValue)[@"state"] intValue] == 1) running = YES;
        }
        if (!running) {
            return 0;
        }
        [NSThread sleepForTimeInterval:0.1];
    }
    return -1;
}

int main(int argc, const char **argv) {
 @autoreleasepool {
    if (argc != 2) return 2;
    NSString *root = [NSString stringWithUTF8String:argv[1]];
    NSXPCConnection *connection = [[NSXPCConnection alloc] initWithServiceName:@"com.xunlei.DownloadService"];
    connection.remoteObjectInterface = [NSXPCInterface interfaceWithProtocol:@protocol(DownloadServiceProtocol)];
    connection.exportedInterface = [NSXPCInterface interfaceWithProtocol:@protocol(DownloadHostAgent)];
    connection.exportedObject = [HostAgent new];
    [connection resume];
    __block BOOL broken = NO;
    id<DownloadServiceProtocol> proxy = [connection synchronousRemoteObjectProxyWithErrorHandler:^(NSError *error) { broken = YES; }];
    __block NSString *version = nil;
    [proxy getVersionwithReply:^(NSString *value) { version = value; }];
    if (broken || !version) return 1;
    output(@{@"protocol":@1,@"version":version});
    NSMutableSet *owned = [NSMutableSet set];
    NSMutableDictionary *tokens = [NSMutableDictionary dictionary];
    NSMutableSet *tokenPending = [NSMutableSet set];
    BOOL initialized = NO;
    char *line = NULL; size_t capacity = 0;
    while (getline(&line, &capacity, stdin) > 0) {
      @autoreleasepool {
        if (strlen(line) > 1024 * 1024) break;
        NSData *data = [[NSString stringWithUTF8String:line] dataUsingEncoding:NSUTF8StringEncoding];
        NSDictionary *r = [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL];
        if (![r isKindOfClass:[NSDictionary class]]) break;
        NSString *op = r[@"op"]; NSNumber *task = r[@"taskId"];
        __block int code = -1;
        NSMutableDictionary *reply = [NSMutableDictionary dictionary];
        if ([op isEqual:@"init"]) {
            NSString *appVersion = r[@"appVersion"];
            NSNumber *clientVersion = r[@"clientVersion"];
            if (![appVersion isKindOfClass:NSString.class] || !appVersion.length ||
                ![clientVersion isKindOfClass:NSNumber.class] || clientVersion.integerValue <= 0) {
                output(@{@"code": @(-1)});
                continue;
            }
            NSDictionary *context = @{@"appVersion":appVersion,@"clientVersion":clientVersion,
                @"configPath":root,@"downloadPath":root,@"peerId":r[@"peerId"],@"guid":r[@"peerId"],@"networkType":@5};
            [proxy initETM:context withReply:^(int v) { code = v; }];
            initialized = code == 0;
            if (initialized) {
                [owned addObjectsFromArray:r[@"taskIds"] ?: @[]];
                NSMutableArray *running = [NSMutableArray array];
                for (NSNumber *idNumber in owned) {
                    if ([infoFor(proxy, idNumber.unsignedIntValue)[@"state"] intValue] == 1) [running addObject:idNumber];
                }
                code = pauseAndWait(proxy, running);
            }
        } else if ([op isEqual:@"identity"] && initialized) {
            [proxy setUserInfo:@{@"userId":r[@"userId"],@"vipType":r[@"vipType"] ?: @0} withReply:^(int v) {code=v;}];
        } else if ([op isEqual:@"limits"] && initialized) {
            [proxy setMaxDownloadSpeed:[r[@"download"] longLongValue] maxUploadSpeed:[r[@"upload"] longLongValue] withReply:^(int v) {code=v;}];
        } else if ([op isEqual:@"create"] && initialized) {
            NSDictionary *parameters = @{@"taskType":@0,@"kind":@1,@"source":@"versadown",
                @"url":r[@"url"],@"fileName":r[@"name"],@"saveDirPath":r[@"directory"],
                @"refURL":r[@"referer"] ?: @"",@"userAgent":r[@"userAgent"] ?: @"Mozilla/5.0",
                @"fileSize":r[@"size"] ?: @0};
            __block int created = 0;
            [proxy createTask:parameters withReply:^(int v, int identifier) {code=v;created=identifier;}];
            if (code == 0 && created > 0) {
                task = @(created); [owned addObject:task]; reply[@"taskId"] = task;
            }
        } else if ([op isEqual:@"poll"] && [owned containsObject:task]) {
            NSDictionary *info = infoFor(proxy, task.unsignedIntValue);
            __block NSDictionary *bytes = nil;
            [proxy getTaskResourceRecvBytes:task.unsignedIntValue withReply:^(NSDictionary *v) {bytes=v;}];
            NSMutableDictionary *safe = [NSMutableDictionary dictionary];
            for (NSString *key in @[@"taskName",@"taskDir",@"state",@"downloadedSize",@"fileSize",@"failedCode",@"originErrorCode",@"usingNumbers"])
                if (info[key]) safe[key] = info[key];
            reply[@"info"] = safe;
            NSMutableDictionary *counters = [NSMutableDictionary dictionary];
            for (NSString *key in @[@"DSKeyTaskRecvTotalBytes",@"DSKeyTaskRecvOriginBytes",@"DSKeyTaskRecvDCDNBytes",@"DSKeyTaskRecvP2PBytes",@"DSKeyTaskRecvP2SBytes"])
                if ([bytes[key] isKindOfClass:[NSNumber class]]) counters[key] = bytes[key];
            reply[@"bytes"] = counters; code = info.count ? 0 : -1;
            // The restored SDK task must be running before a token can be attached.
            if ([tokenPending containsObject:task] && [info[@"state"] intValue] == 1) {
                __block int tokenCode = -1;
                [proxy setAccelerateToken:tokens[task] accelerateType:3 taskId:task.unsignedIntValue subIndex:0 withReply:^(int v) {tokenCode=v;}];
                [tokenPending removeObject:task]; reply[@"accelerationCode"] = @(tokenCode);
            }
        } else if ([op isEqual:@"token"] && [owned containsObject:task]) {
            if ([r[@"token"] isKindOfClass:[NSString class]] && [r[@"token"] length]) {
                tokens[task] = r[@"token"]; [tokenPending addObject:task]; code = 0;
            }
        } else if ([op isEqual:@"resume"] && [owned containsObject:task]) {
            [proxy resumeTasks:@[task] withReply:^(int v) {code=v;}];
            if (code == 0 && r[@"url"]) {
                // Restored tasks are recreated lazily by resumeTasks. Updating a
                // paused task first returns 9104 and leaves the old URL in place.
                for (int attempt = 0; attempt < 10; attempt++) {
                    [proxy changeTasksOriginURL:r[@"url"] taskId:task.unsignedIntValue subId:0 withReply:^(int v) {code=v;}];
                    if (code != 9104) break;
                    [NSThread sleepForTimeInterval:0.1];
                }
                if (code == 0) [proxy resumeTasks:@[task] withReply:^(int v) {code=v;}];
                if (code != 0) pauseAndWait(proxy, @[task]);
            }
            if (tokens[task]) [tokenPending addObject:task];
        } else if (([op isEqual:@"pause"] || [op isEqual:@"cancel"]) && [owned containsObject:task]) {
            code = pauseAndWait(proxy, @[task]);
            if (code == 0 && [op isEqual:@"cancel"]) {
                [tokens removeObjectForKey:task]; [tokenPending removeObject:task];
            }
        } else if ([op isEqual:@"forget"] && [owned containsObject:task]) {
            pauseAndWait(proxy, @[task]);
            [proxy destroyTasks:@[task] shouldRemoveFile:NO withReply:^(int v) {code=v;}];
            if (code == 0) { [owned removeObject:task]; [tokens removeObjectForKey:task]; [tokenPending removeObject:task]; }
        } else if ([op isEqual:@"shutdown"]) { break; }
        reply[@"code"] = @(broken ? -1 : code); output(reply);
        if (broken) break;
      }
    }
    free(line);
    if (initialized && !broken) {
        pauseAndWait(proxy, owned.allObjects);
        [proxy uninitETM];
        // uninitETM has no reply. A following round trip lets the service finish
        // flushing its checkpoint before the host invalidates the connection.
        [proxy getVersionwithReply:^(NSString *value) {}];
    }
    [connection invalidate]; return 0;
 }
}
