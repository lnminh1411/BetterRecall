// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Tests;

/// <summary>Bug sweep 3, batch L: one "Model not installed" log line per op and model, not one per keyframe (D19).</summary>
public sealed class Sweep3LWorkerTests
{
    [Fact]
    public void Missing_model_failure_is_logged_once_per_op_and_model()
    {
        var h = new RequestHandler(new FakeEngine(), () => null!);
        var missing = WorkerErrors.ModelNotInstalled("ocr-paddle");
        Assert.True(h.ShouldLogFailure("ocr", missing));
        Assert.False(h.ShouldLogFailure("ocr", missing));
        Assert.False(h.ShouldLogFailure("ocr", missing));
        Assert.True(h.ShouldLogFailure("ner", missing));                                       // another op
        Assert.True(h.ShouldLogFailure("ocr", WorkerErrors.ModelNotInstalled("ocr-windows")));  // another model
        // other failures are logged every time
        Assert.True(h.ShouldLogFailure("ocr", "Bad request."));
        Assert.True(h.ShouldLogFailure("ocr", "Bad request."));
        // a new worker start (new handler) logs it again
        Assert.True(new RequestHandler(new FakeEngine(), () => null!).ShouldLogFailure("ocr", missing));
    }
}
