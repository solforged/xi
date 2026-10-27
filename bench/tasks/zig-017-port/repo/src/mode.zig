const std = @import("std");
const builtin = @import("builtin");

/// Short label for an optimization mode, used in build banners.
pub fn modeLabel(mode: std.builtin.OptimizeMode) []const u8 {
    return switch (mode) {
        .Debug => "dev",
        .ReleaseSafe => "checked",
        .ReleaseFast => "speed",
        .ReleaseSmall => "size",
    };
}

/// Whether this binary was built for speed or size rather than safety.
pub fn isOptimized() bool {
    return builtin.mode == .ReleaseFast or builtin.mode == .ReleaseSmall;
}

/// `arch-os-abi (format)` for the compilation target.
pub fn targetSummary(buf: []u8) ![]const u8 {
    return std.fmt.bufPrint(buf, "{s}-{s}-{s} ({s})", .{
        @tagName(builtin.cpu.arch),
        @tagName(builtin.os.tag),
        @tagName(builtin.abi),
        @tagName(builtin.object_format),
    });
}
