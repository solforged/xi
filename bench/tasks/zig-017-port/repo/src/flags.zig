const std = @import("std");

pub const Feature = enum { color, unicode, mouse, paste };

/// Open ports among the first sixteen.
pub const Ports = std.bit_set.IntegerBitSet(16);

/// Busy lanes in a 256-lane scheduler.
pub const Lanes = std.bit_set.ArrayBitSet(usize, 256);

pub fn allFeatures() std.EnumSet(Feature) {
    return std.EnumSet(Feature).initFull();
}

pub fn noFeatures() std.EnumSet(Feature) {
    return std.EnumSet(Feature).initEmpty();
}

pub fn closedPorts() Ports {
    return Ports.initEmpty();
}

pub fn busyLanes() Lanes {
    return Lanes.initFull();
}

/// Marks each id as seen in a set of `len` bits. The caller frees it.
pub fn markSeen(
    allocator: std.mem.Allocator,
    ids: []const usize,
    len: usize,
) !std.DynamicBitSetUnmanaged {
    var seen = try std.DynamicBitSetUnmanaged.initEmpty(allocator, len);
    for (ids) |id| seen.set(id);
    return seen;
}
