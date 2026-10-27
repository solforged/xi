const std = @import("std");

pub const Entry = struct {
    key: []const u8,
    value: u32,
};

/// `key=value` for one entry, allocated in `arena`.
pub fn line(arena: std.mem.Allocator, entry: Entry) ![]u8 {
    return std.fmt.allocPrint(arena, "{s}={d}", .{ entry.key, entry.value });
}

/// Pages needed for `total` rows at `per_page` rows per page.
pub fn pages(total: usize, per_page: usize) usize {
    return std.math.divCeil(usize, total, per_page) catch unreachable;
}

/// A row of 32 spaces.
pub fn blankRow() [32]u8 {
    return [1]u8{' '} ** 32;
}

/// Whether `key` appears in `text`, ignoring ASCII case.
pub fn hasKey(text: []const u8, key: []const u8) bool {
    return std.ascii.indexOfIgnoreCase(text, key) != null;
}

/// Appends one line per entry. Short lines are built on the stack.
pub fn render(gpa: std.mem.Allocator, entries: []const Entry, out: *std.ArrayList(u8)) !void {
    var fallback = std.heap.stackFallback(512, gpa);
    const scratch = fallback.get();
    for (entries) |entry| {
        const text = try line(scratch, entry);
        defer scratch.free(text);
        try out.appendSlice(gpa, text);
        try out.append(gpa, '\n');
    }
}

/// Renders every entry and logs the error when rendering fails.
pub fn writeAll(gpa: std.mem.Allocator, entries: []const Entry, out: *std.ArrayList(u8)) !void {
    errdefer |err| std.log.err("report failed: {s}", .{@errorName(err)});
    try render(gpa, entries, out);
    try out.appendSlice(gpa, "--\n");
}

/// Entries without a value are skipped.
pub fn skip(entry: Entry) void {
    _ = entry;
    return void{};
}
