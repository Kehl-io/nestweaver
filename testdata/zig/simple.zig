const std = @import("std");
const math = @import("math.zig");

pub const SensorKind = enum {
    temperature,
    humidity,
    pressure,
};

pub const SensorConfig = struct {
    name: []const u8,
    kind: SensorKind,
    threshold: f64,
};

const InternalState = union {
    active: bool,
    error_code: u32,
};

pub fn initialize(config: SensorConfig) void {
    std.debug.print("Initializing sensor: {s}\n", .{config.name});
    calibrate(config);
}

fn calibrate(config: SensorConfig) void {
    _ = config;
}

pub fn main() !void {
    const config = SensorConfig{
        .name = "temp-1",
        .kind = .temperature,
        .threshold = 100.0,
    };
    initialize(config);
}
