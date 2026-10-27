const std = @import("std");

pub const Config = struct {
    name: []const u8 = "xi",
    width: u16 = 80,
    color: bool = true,
};

/// Number of fields in struct type `T`.
pub fn fieldCount(comptime T: type) usize {
    return @typeInfo(T).@"struct".fields.len;
}

/// Writes `name: type` for each field of `value`'s struct type.
pub fn describe(w: *std.Io.Writer, value: anytype) !void {
    const T = @TypeOf(value);
    inline for (@typeInfo(T).@"struct".fields) |field| {
        try w.print("{s}: {s}\n", .{ field.name, @typeName(field.type) });
    }
}
