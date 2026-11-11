// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Tests;

public class FramingTests
{
    private static MemoryStream S(string s) => new(Encoding.UTF8.GetBytes(s));
    private static MemoryStream S(byte[] b) => new(b);

    [Fact]
    public async Task Exactly_4096_bytes_is_accepted_4097_is_rejected()
    {
        var ok = new string('a', 4096) + "\n";
        Assert.Equal(4096, (await LineProtocol.ReadLineAsync(S(ok), 4096, default))!.Length);
        var bad = new string('a', 4097) + "\n";
        var ex = await Assert.ThrowsAsync<LineTooLongException>(() => LineProtocol.ReadLineAsync(S(bad), 4096, default));
        Assert.Equal(LineProtocol.TooLongError, ex.Message);
    }

    [Fact]
    public async Task Bom_and_cr_are_stripped()
    {
        var bytes = new byte[] { 0xEF, 0xBB, 0xBF }.Concat(Encoding.UTF8.GetBytes("ping\r\nversion\r\n")).ToArray();
        var s = S(bytes);
        Assert.Equal("ping", await LineProtocol.ReadLineAsync(s, 4096, default));
        Assert.Equal("version", await LineProtocol.ReadLineAsync(s, 4096, default));
        Assert.Null(await LineProtocol.ReadLineAsync(s, 4096, default));
    }

    [Fact]
    public async Task Last_line_without_newline_and_utf8()
    {
        var s = S("search cà phê 東京");
        Assert.Equal("search cà phê 東京", await LineProtocol.ReadLineAsync(s, 4096, default));
        Assert.Null(await LineProtocol.ReadLineAsync(s, 4096, default));
    }

    [Fact]
    public async Task Empty_lines_come_back_empty()
    {
        var s = S("\n\r\nping\n");
        Assert.Equal("", await LineProtocol.ReadLineAsync(s, 4096, default));
        Assert.Equal("", await LineProtocol.ReadLineAsync(s, 4096, default));
        Assert.Equal("ping", await LineProtocol.ReadLineAsync(s, 4096, default));
    }

    [Fact]
    public void Envelope_shapes()
    {
        Assert.Equal("{\"ok\":true,\"data\":null}", Envelope.Ok("null"));
        Assert.Equal("{\"ok\":false,\"error\":\"Store is locked.\"}", Envelope.Error("Store is locked."));
        var (ok, _, err) = Envelope.Parse(Envelope.Error("x"));
        Assert.False(ok);
        Assert.Equal("x", err);
    }
}
