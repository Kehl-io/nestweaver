#import <Foundation/Foundation.h>
#import "Greeter.h"
#include "utils.h"

@protocol GreeterProtocol
- (NSString *)greet:(NSString *)name;
@end

@interface SimpleGreeter : NSObject <GreeterProtocol>
@property (nonatomic, strong) NSString *prefix;
@end

@implementation SimpleGreeter

- (instancetype)initWithPrefix:(NSString *)prefix {
    self = [super init];
    if (self) {
        _prefix = prefix;
    }
    return self;
}

- (NSString *)greet:(NSString *)name {
    NSString *formatted = [self formatName:name];
    return [NSString stringWithFormat:@"%@ %@!", self.prefix, formatted];
}

- (NSString *)formatName:(NSString *)name {
    return [name capitalizedString];
}

@end

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        SimpleGreeter *greeter = [[SimpleGreeter alloc] initWithPrefix:@"Hello"];
        NSString *result = [greeter greet:@"world"];
        NSLog(@"%@", result);
    }
    return 0;
}
