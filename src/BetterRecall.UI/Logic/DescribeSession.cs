// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

/// <summary>
/// One cloud describe dialog. The agent keeps describe tokens per connection, so the preview and the confirmed send go
/// out on the same connection, opened for this dialog and closed with it (B2: the send used to go out on a new
/// connection and always answered "Invalid or expired token."). The preview waits for the agent's OCR redaction (up
/// to 30 s); on its own connection that wait no longer holds up thumbnails on the media connection (AB-S1).
/// </summary>
public sealed class DescribeSession(AgentClient client) : IAsyncDisposable
{
    private readonly AgentApi _api = new(client);

    /// <summary>The preview's token, kept for the send. Null before a preview or after a failed one.</summary>
    public DescribePreview? Preview { get; private set; }

    public async Task<AgentResult<DescribePreview>> PreviewAsync(string id, CancellationToken ct = default)
    {
        var r = await _api.DescribePreviewAsync(id, ct).ConfigureAwait(false);
        Preview = r.Ok ? r.Value : null;
        return r;
    }

    /// <summary>Sends the image the preview showed. Without a preview there is no token to send.</summary>
    public async Task<AgentResult<DescribeResult>> SendAsync(CancellationToken ct = default)
    {
        var token = Preview?.Token;
        if (token is null) return new AgentResult<DescribeResult>(false, null, AgentErrorKind.Server, AgentErrors.InvalidToken);
        Preview = null;   // single use, as on the agent
        return await _api.DescribeSendAsync(token, ct).ConfigureAwait(false);
    }

    public ValueTask DisposeAsync()
    {
        Preview = null;
        return client.DisposeAsync();
    }
}
